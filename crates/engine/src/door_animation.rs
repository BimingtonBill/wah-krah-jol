//! Load doors that open with their own model's animation: the state machine behind
//! [`DoorState`](crate::doors::DoorState).
//!
//! A door model's NIF carries the `Open` and `Close` controller sequences that swing it, and the
//! converter bakes them into glTF animation clips on the same `.glb` the model is loaded from.
//! This module finds those clips on a load door's model, builds the door an
//! [`AnimationGraph`] of its own - the glTF loader gives the spawned scene an [`AnimationPlayer`]
//! but no graph, and `advance_animations` needs both - and drives
//! `Closed -> Opening -> Open -> Closing` from `E` ([`OpenDoor`](crate::transition::OpenDoor)) and
//! from the playing clip's own clock.
//!
//! # A door with no animation
//!
//! Most door models carry no clips at all (596 of the 768 door NIFs in this install), and until the
//! assets are reconverted none does. Such a door is promoted to [`DoorState::Open`] in the frame it
//! is activated: the doorway opens in the same frame the player asks for it, which is what a door
//! did before there were clips.
//!
//! # The door that barely moves, and the twin it borrows its swing from
//!
//! A load door is the door the game hides behind a loading screen, so its own `Open` clip only has
//! to move the leaf far enough to be seen before the screen comes down: the demo route's
//! `DweDoorLarge01Load` turns its two leaves 5.4 and 8.7 degrees. The engine has no loading screen,
//! and a door that turns 5 degrees is a wall with a wall behind it - while the game ships a
//! non-load twin of the same door (`dwemerlargedoor01.nif` beside `dwemerlargedoorload01.nif`)
//! whose `Open` clip is the real 86-degree swing.
//!
//! So when a door's own `Open` clip does not clear the doorway
//! ([`DOORWAY_CLEAR_DEGREES`](crate::doors::DOORWAY_CLEAR_DEGREES)), the engine gives it a swing in
//! two steps.
//!
//! First it looks the twin up - the same model path with the marker removed ([`twin_model_path`]) -
//! and plays the twin's `Open`/`Close` clips instead, rebuilt onto the door's own nodes, because an
//! [`AnimationTargetId`] is a hash of a node's whole name path and the two models name their roots
//! differently. A twin is used only when every node its clips move is a node the door's own model
//! has, and that is the exception rather than the rule: 17 of the install's 19 twinned pairs name
//! their leaves something else entirely - the demo route's own door turns `Object02` and `Object43`
//! and its twin turns `Object45` and `Object47` (`tools/research/load_door_twin_coverage.py pairs`).
//!
//! Then, for every door the twin cannot help, the door's **own** clip is scaled up until it opens
//! the doorway ([`swung_open_clips`]): the nodes, the hinge each leaf turns about and the way it
//! turns are already in the clip, and only the distance is short, so the whole clip is turned
//! `factor` times as far from the pose it starts on. That is what gives every load door its swing,
//! and it changes nothing but how far the door opens. A clip that already clears the doorway is
//! never touched, and a door with no clip at all stays the static door it is.
//!
//! # What is drawn while a door opens
//!
//! The leaf is the point of the animation, so it is drawn while the `Open` clip plays: a door whose
//! leaf vanished the moment it was asked to open would be no better than the teleport this
//! replaces. What happens when the clip is done depends on how far it turned the leaf
//! ([`DOORWAY_CLEAR_DEGREES`](crate::doors::DOORWAY_CLEAR_DEGREES)):
//!
//! * a leaf that swung clear of the opening (a Nordic or Imperial door, 115-135 degrees) stays
//!   drawn, swung open, like a real door;
//! * a leaf that did not (the demo route's `DweDoorLarge01Load` turns its leaves 5-9 degrees) is
//!   hidden at `Open`, so the doorway is the opening the leaf failed to make - and only the leaves
//!   are hidden, never the frame, which is what keeps the doorway looking like a doorway;
//! * a door with no clip of its own has no leaf that can move at all, and there the whole model has
//!   to go - but that line is the portal's to write, from
//!   [`hides_whole_reference`](crate::doors::DoorState::hides_whole_reference).
//!
//! While the `Close` clip plays the leaves are drawn again from its first frame: a leaf that is
//! coming back is a leaf.
//!
//! # The far door of a crossing
//!
//! A crossing does not only move the camera: a [`CrossDoor`] through a doorway-anchored door lands
//! the player at the **destination doorway's plane** (`crate::transition`), which is where the far
//! door of the link stands, so that door has to be out of the way in the frame the player arrives
//! ([`OpenDestinationDoor`], [`open_arrival_doors`]). It is opened the way the window showed it - at
//! the point of its own `Open` clip that the source door's own clip had reached - and it is left
//! open afterwards, which is also what lets the portal render back through it once the player has
//! walked on.
//!
//! # What the rest of the engine reads
//!
//! [`DoorState`](crate::doors::DoorState) sits on the door reference next to
//! [`LoadDoor`](crate::doors::LoadDoor) and is the one answer to "is this door open":
//! [`is_open`](crate::doors::DoorState::is_open) is true from the first frame of the swing onward,
//! and that is what the crossing gates on.
//! [`DoorLeaf`](crate::doors::DoorLeaf) marks the moving part of the model - the nodes the clips
//! have curves for, not the frame around them - and
//! [`mesh_is_out_of_the_way`](crate::doors::mesh_is_out_of_the_way) is what the walk probe asks
//! about a mesh it hit: a drawn leaf mid-swing must not trap the player.
//!
//! # What wires this module up
//!
//! `PortalPlugin` adds this plugin for interactive runs (`docs/design/portal-plugin.md`), so a run
//! that is looked at rather than measured has it and a benchmark run does not:
//!
//! ```ignore
//! app.add_plugins(crate::door_animation::DoorAnimationPlugin);
//! ```
//!
//! The rest of the wiring is one line each in the modules that ask the question:
//! `portal::show_load_door_leaves` hides the leaf of an open door and the whole model of a *static*
//! one, `player::player_walk` skips a leaf its door has taken out of the doorway, and
//! [`crate::transition::door_is_open`] - which the portal's door choice, the player's plane trigger
//! and the demo tour's walk-through all read - is [`DoorState::is_open`].

use crate::{
    doors::{DOORWAY_CLEAR_DEGREES, DoorLeaf, DoorState, LoadDoor, OPEN_FRACTION},
    transition::{CrossingHeld, OpenDestinationDoor, OpenDoor},
    world::components::MeshHandle,
};
use bevy::{
    animation::{
        AnimationClip, AnimationPlayer, AnimationTargetId, animated_field,
        animation_curves::{AnimatableCurve, AnimatableKeyframeCurve},
        graph::{AnimationGraph, AnimationGraphHandle, AnimationNodeIndex},
        transition::AnimationTransitions,
    },
    prelude::*,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// The clip name a door's opening sequence is expected to carry. Matched case-insensitively:
/// `Open` is the convention, `open` exists too, and `Forward`/`Backward` are other names entirely
/// (`tools/research/door_animation_nif.py census`).
const OPEN_CLIP: &str = "open";

/// The clip name a door's closing sequence is expected to carry, matched the same way.
const CLOSE_CLIP: &str = "close";

/// How long a door takes to cross-fade from one clip into the other (design section 2.3).
///
/// A reversal starts the new clip at the pose the old one reached, so the fade covers the mismatch
/// between the two sequences rather than the swing: the demo route's large door opens in 0.6 s and
/// closes in 0.6333 s, and a door that is halfway open when it is told to close is given the rest
/// of the swing as a fresh heading rather than a jump.
const CLIP_FADE: Duration = Duration::from_millis(150);

/// How many frames a door waits for the scene its clips live in before it gives up and stays
/// static.
///
/// The model, its clip sub-assets and the scene the loader spawns are the same file and arrive
/// within a frame or two of each other, so a door still waiting two seconds later is waiting for
/// something that is not coming - a scene that failed to spawn, or an animation root the loader
/// never found. Giving up makes it a static door (the doorway opens in the frame it is activated)
/// instead of a door that never opens at all.
const SCENE_WAIT_FRAMES: u32 = 120;

/// How many frames a load door waits for its non-load twin before giving up on it and scaling its
/// own clip instead ([`twin_swing`]).
///
/// Much shorter than [`SCENE_WAIT_FRAMES`], because of what is being waited for: the door's own
/// scene is the thing the player can see is missing, and a door that never resolves is a door that
/// never opens - while a twin is the second thing tried, a door without one has a swing from its own
/// clip to fall back on, and until it falls back the door is still `Closed`: a player who reaches it
/// in that window gets a door that opens the way it did before any of this. The twin's path is
/// derived from the door's own model path, so its load starts in the same frame the door's own
/// model resolved, and half a second is several times what a converted model takes to arrive.
const TWIN_WAIT_FRAMES: u32 = 30;

/// How far a load door's leaves stand once the engine has given a door whose own clip barely turns
/// them a swing of its own: a door standing open, well past
/// [`DOORWAY_CLEAR_DEGREES`](crate::doors::DOORWAY_CLEAR_DEGREES) so that the doorway is unarguably
/// one and the leaf is visibly out of it.
const SWUNG_OPEN_DEGREES: f32 = 90.0;

/// The most a load door's own clip may be scaled up by ([`swing_scale`]).
///
/// A clip that hardly moves at all - `RiftenRWDoorLoad01` turns its leaf 2.9 degrees, and how far
/// that leaf's hinge is from its pivot is not in the clip - would need 31 times its own swing to
/// stand at [`SWUNG_OPEN_DEGREES`]. A swing blown up that far is inventing motion the model never
/// described, so the leaves stop here (that door's 2.9 degrees becomes 58) rather than being flung
/// past their hinges. Doors whose own swing is within this of the target - 4.5 degrees and up, which
/// is nearly all of them - reach it exactly.
const MAX_SWING_SCALE: f32 = 20.0;

/// The most a door's `Close` clip may start from where its `Open` clip ended, and end from where
/// `Open` started, and still count as the same swing run backwards ([`close_mirrors_open`]), in
/// degrees. The install's doors agree to a hundredth of a degree; this is the width of "the same".
const CLOSE_MIRROR_DEGREES: f32 = 1.0;

/// How many samples a door's own clip is rebuilt with per second of its length when it is scaled or
/// reversed.
const SWING_SAMPLES_PER_SECOND: f32 = 60.0;

/// The fewest and the most samples a rebuilt clip gets: a clip shorter than a frame is not a swing
/// at all, and a long one - the census's longest door sequence is 17.9 seconds - is not worth a
/// thousand keys to reproduce sample for sample.
const MIN_SWING_SAMPLES: u32 = 8;
const MAX_SWING_SAMPLES: u32 = 120;

/// The token in a load door's file name, and in the name of its model's own root node, that makes
/// it the door the game hides behind a loading screen. Its non-load twin - the model with the real
/// swing in it - is the same name with this taken out ([`twin_model_path`]).
const LOAD_MARKER: &str = "load";

/// The path of the model a load door would have if it were not a load door, or `None` when its own
/// path carries no [`LOAD_MARKER`] to take out - and so has no twin to look for.
///
/// The rule is read off the install rather than off one example
/// (`tools/research/load_door_twin_coverage.py`): of the 103 models Skyrim's load doors use, 31
/// carry a `load` marker in the file name and, for 19 of those, the same name without it is a model
/// that exists and is converted. The markers themselves are `Load01` (658 door references),
/// `LoadMarker01` (315, every one of them an invisible auto-load marker), `Load02` (123), `Load`
/// (54), `LoadUp01` (23), `LoadDown01` (21), `LoadDoor01` (2) and `LoadExt` (2) - always a `load`
/// immediately before the token that distinguishes the load door, which is why taking out the first
/// `load` of the file stem is the whole rule. The marker is case-insensitive (`AutoLoadMarker01`).
fn twin_model_path(model_path: &str) -> Option<String> {
    let (directory, file) = match model_path.rfind(['/', '\\']) {
        Some(separator) => model_path.split_at(separator + 1),
        None => ("", model_path),
    };
    let (stem, extension) = match file.rfind('.') {
        Some(dot) => file.split_at(dot),
        None => (file, ""),
    };
    Some(format!(
        "{directory}{}{extension}",
        strip_load_marker(stem)?
    ))
}

/// `name` with its first `load` - whatever its case - taken out, or `None` when it carries none.
///
/// This is the marker rule in both the places it is needed: on a model's file name, to find the
/// twin ([`twin_model_path`]), and on the name of the model's own root node, to turn a door node's
/// [`AnimationTargetId`] into the one the twin's clips use for it ([`twin_target_ids`]).
fn strip_load_marker(name: &str) -> Option<String> {
    let start = name.to_ascii_lowercase().find(LOAD_MARKER)?;
    let end = start + LOAD_MARKER.len();
    let stripped = format!("{}{}", &name[..start], &name[end..]);
    (!stripped.is_empty()).then_some(stripped)
}

/// The animation a load door's model resolved to, built once the model's `Gltf` asset and its clips
/// have loaded. Its absence means "still being worked out"; a door with every field empty is a
/// static leaf, and one with no clips at all never gets a clip to play.
#[derive(Component, Debug, Clone, Copy, Default)]
struct DoorAnimation {
    /// The entity the glTF loader put the door's [`AnimationPlayer`] on: the animation root of the
    /// spawned scene, a descendant of the door reference. `None` for a model with no clips.
    player: Option<Entity>,
    /// The door's `Open` clip and the graph node that plays it, if the model has one.
    open: Option<DoorClip>,
    /// The door's `Close` clip, if the model has one. A model with only `Open` plays that and never
    /// closes (design section 5).
    close: Option<DoorClip>,
    /// Whether the `Open` clip swings the leaves far enough to leave the doorway open
    /// ([`DOORWAY_CLEAR_DEGREES`]). False for a static door, and for a clip that barely turns its
    /// leaves: both leave a doorway that is not one, so the leaves are hidden when the door is
    /// `Open`.
    clears_doorway: bool,
}

/// One of a door's clips: the node of the door's own animation graph that plays it, and its length.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DoorClip {
    /// The graph node [`AnimationPlayer::start`] takes to play this clip.
    node: AnimationNodeIndex,
    /// The clip's length in seconds (`AnimationClip::duration`), the denominator of
    /// [`OPEN_FRACTION`].
    seconds: f32,
}

/// The door model's root `Gltf` asset is loading, or has loaded and is waiting for the scene its
/// clips live in. The clips are named sub-assets of the same file, and `Gltf::named_animations` is
/// what says which of them exist.
///
/// It is also where the door waits for its non-load twin, once its own `Open` clip has turned out
/// not to clear the doorway: the twin is loaded only for such a door, so a door that swings clear
/// on its own never pays for the second model.
#[derive(Component, Debug, Clone)]
struct PendingDoorModel {
    /// The model's root `Gltf` asset.
    model: Handle<Gltf>,
    /// The path the model was loaded from, which the twin's path is derived from.
    path: String,
    /// The door's non-load twin, once its own clip has been read and found too narrow: `None` while
    /// the door has not needed one yet, which is most doors.
    twin: Option<TwinModel>,
    /// Frames spent waiting since this stage of the door's resolution began - for the model's clip
    /// assets and the scene's [`AnimationPlayer`] ([`SCENE_WAIT_FRAMES`]), and then for the twin
    /// ([`TWIN_WAIT_FRAMES`]). A door that waits longer gives up on what it is waiting for: a static
    /// door, or one that gets its swing from its own clip.
    waiting: u32,
}

/// A door's non-load twin, loading or loaded: the model whose `Open`/`Close` clips are the door's
/// real swing.
#[derive(Component, Debug, Clone)]
struct TwinModel {
    /// The twin's path, as [`twin_model_path`] derived it - kept for the log line.
    path: String,
    /// The twin model's root `Gltf` asset.
    model: Handle<Gltf>,
}

/// What the engine did to a load door whose own `Open` clip does not clear the doorway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwingSource {
    /// The door's non-load twin's clips, rebuilt onto the door's own nodes
    /// ([`borrow_swing`]).
    Twin,
    /// The door's own clip, turned further than it was baked to turn
    /// ([`swung_open_clips`]).
    Scaled,
}

/// What the engine has done to each door model it has had to help, so that the log says what
/// happened to a model once rather than once per door of it.
#[derive(Resource, Debug, Default)]
struct AdjustedSwings(HashMap<String, SwingSource>);

/// What an animating door model turned out to be: the clips to play, the entity the loader put the
/// door's [`AnimationPlayer`] on, and the nodes the clips move.
///
/// It is built the same way whether the clips are the model's own or a twin's rebuilt onto its
/// nodes, which is what keeps the state machine, the leaf marking and the graph out of that
/// question.
#[derive(Debug)]
struct ResolvedModel {
    player: Entity,
    /// The `Open` clip.
    open: Handle<AnimationClip>,
    /// The `Close` clip, or `None` for a model that has none.
    close: Option<Handle<AnimationClip>>,
    /// The `Open` clip's length in seconds.
    open_seconds: f32,
    /// How far the `Open` clip turns the leaves, in degrees: the measure behind
    /// [`DoorAnimation::clears_doorway`].
    open_swing: f32,
    /// The `Close` clip's length in seconds, or `None` for a model that has no `Close` clip.
    close_seconds: Option<f32>,
    /// The animation targets the clips have curves for: the leaves.
    moved: HashSet<AnimationTargetId>,
}

/// A twin's clips rebuilt onto the door's own nodes, with what replaces the twin's own measurement
/// of its swing - the rebuilt clips are what plays, so they are what says whether the doorway opens.
#[derive(Debug)]
struct BorrowedClips {
    open: AnimationClip,
    close: Option<AnimationClip>,
    /// The rebuilt `Open` clip's length in seconds.
    seconds: f32,
    /// How far the rebuilt `Open` clip turns the door's leaves, in degrees.
    swing: f32,
    /// The rebuilt `Close` clip's length in seconds, or `None` when the twin has no `Close` clip.
    close_seconds: Option<f32>,
    /// The door's own nodes the rebuilt clips move.
    moved: HashSet<AnimationTargetId>,
}

/// A load door reference that has not been given its animation yet: the door itself, and the model
/// path to look the clips up on.
type UnresolvedDoorQuery<'world, 'state> = Query<
    'world,
    'state,
    (Entity, &'static LoadDoor, Option<&'static MeshHandle>),
    (Without<DoorAnimation>, Without<PendingDoorModel>),
>;

/// Lets a load door open with its own model's `Open`/`Close` animation.
///
/// Added by [`PortalPlugin`](crate::portal::PortalPlugin) for the runs that are looked at rather
/// than measured. It needs nothing but the asset server: a run that never writes [`OpenDoor`] never
/// changes a door, so a `--shots` run is unaffected.
pub struct DoorAnimationPlugin;

impl Plugin for DoorAnimationPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<OpenDoor>()
            // Written by `crate::transition`'s crossing (and registered there too): this plugin is
            // the one that opens the door, so it is the one that reads the message - a run that adds
            // this plugin without the transition has no crossing and never sees one.
            .add_message::<OpenDestinationDoor>()
            .init_resource::<AdjustedSwings>()
            .init_resource::<ArrivalOpenings>()
            .add_systems(
                Update,
                (
                    request_door_models,
                    attach_door_animations,
                    activate_doors,
                    // After the crossing that asks for it, and before the portal draws anything:
                    // the far door of a mapped crossing has to be open in the frame the player
                    // arrives in its doorway, which is the frame the crossing is applied in - and
                    // the portal's own answer for that door, and the window it may open through it,
                    // are read off that state in the same frame.
                    open_arrival_doors
                        .after(crate::transition::DoorTransition)
                        .before(crate::portal::PortalFrame),
                    advance_door_states,
                    update_door_leaves,
                )
                    .chain(),
            );
    }
}

/// Gives every load door its [`DoorState`] and starts loading the model its `Open`/`Close` clips
/// live in.
///
/// The model path is the one [`streaming.rs`](crate::streaming) already converted and loaded the
/// scene from ([`MeshHandle`]): the root `Gltf` asset of that path is where the named animations
/// are, and the loader produces it for the scene sub-asset either way, so asking for it costs a
/// handle rather than a second read.
///
/// An auto-load door is born [`DoorState::Open`]: it is an invisible marker with no leaf, the
/// player is crossed by walking into it ([`crate::player`]), and the crossing reads this state to
/// decide whether a door may be walked through.
fn request_door_models(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    doors: UnresolvedDoorQuery,
) {
    for (entity, door, model) in &doors {
        if door.auto_load {
            commands.entity(entity).try_insert((
                DoorState::Open { animated: false },
                DoorAnimation::default(),
            ));
            continue;
        }
        commands.entity(entity).try_insert(DoorState::Closed);
        match model {
            Some(MeshHandle(path)) => {
                commands.entity(entity).try_insert(PendingDoorModel {
                    model: asset_server.load(path.clone()),
                    path: path.clone(),
                    twin: None,
                    waiting: 0,
                });
            }
            // Nothing to animate: a load door whose base has no model is a static door.
            None => {
                commands.entity(entity).try_insert(DoorAnimation::default());
            }
        }
    }
}

/// Reads the clips off a door's model and hands the door its own animation graph - or, for a door
/// whose own `Open` clip does not clear the doorway, the clips of its non-load twin, rebuilt onto
/// the door's nodes.
///
/// The graph goes on the entity the loader gave the [`AnimationPlayer`]
/// (`bevy_gltf-0.19.0/src/loader/mod.rs#L1093` inserts the player and no graph), along with the
/// [`AnimationTransitions`] that fade one clip into the other when the door reverses. The graph's
/// node indices are what the state machine plays, and the nodes the clips move are marked
/// [`DoorLeaf`] here, because this is the only place that has both the clips and the spawned scene
/// in hand.
///
/// Everything this needs arrives a frame or more after the reference is spawned - the model asset,
/// its clip sub-assets, the scene's nodes, and for a narrow door its twin on top of those - so a
/// door that cannot be finished yet is left pending and tried again next frame, up to
/// [`SCENE_WAIT_FRAMES`] for its own scene and [`TWIN_WAIT_FRAMES`] for a twin.
#[allow(clippy::too_many_arguments)]
fn attach_door_animations(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    models: Res<Assets<Gltf>>,
    mut clips: ResMut<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut adjustments: ResMut<AdjustedSwings>,
    mut doors: Query<(Entity, &LoadDoor, &mut PendingDoorModel), Without<DoorAnimation>>,
    children: Query<&Children>,
    names: Query<&Name>,
    players: Query<(), With<AnimationPlayer>>,
    targets: Query<&AnimationTargetId>,
) {
    for (door, door_row, mut pending) in &mut doors {
        let Some(model) = models.get(&pending.model) else {
            // The model's own load is not this module's problem: `MeshHandle` only exists once
            // `streaming.rs` has a converted model, and the scene is streamed from it either way.
            continue;
        };
        let (open, close) = door_clips(model);
        let Some(open) = open else {
            // The model names no clips at all: a static leaf, and nothing left to wait for. There
            // is nothing to borrow onto either: the loader gives a scene with no animations no
            // `AnimationPlayer` and no `AnimationTargetId`s to play onto
            // (`bevy_gltf-0.19.0/src/loader/mod.rs#L1090`, `#L1545`).
            commands
                .entity(door)
                .try_insert(DoorAnimation::default())
                .try_remove::<PendingDoorModel>();
            continue;
        };
        let Some(resolved) =
            resolve_model(door, &open, close.as_ref(), &clips, &children, &players)
        else {
            pending.waiting += 1;
            if pending.waiting < SCENE_WAIT_FRAMES {
                continue;
            }
            warn!(
                door = format_args!("{:08X}", door_row.ref_id),
                "no animation player in the door's scene after {SCENE_WAIT_FRAMES} frames; \
                 treating it as a static door"
            );
            commands
                .entity(door)
                .try_insert(DoorAnimation::default())
                .try_remove::<PendingDoorModel>();
            continue;
        };

        // The door's own clip opens the doorway: most doors' do, and those never look for a twin.
        // One that does not is a load door in a hurry to be hidden by a loading screen, and its
        // non-load twin is where the real swing is.
        let borrowed = if resolved.open_swing >= DOORWAY_CLEAR_DEGREES {
            None
        } else {
            match twin_swing(
                &mut pending,
                resolved.player,
                &asset_server,
                &models,
                &clips,
                &children,
                &names,
                &targets,
            ) {
                TwinSwing::Waiting => continue,
                TwinSwing::NoTwin => None,
                TwinSwing::Unusable(twin) => {
                    debug!(
                        door = format_args!("{:08X}", door_row.ref_id),
                        twin = %twin,
                        "no swing to borrow; keeping the door's own clip"
                    );
                    None
                }
                TwinSwing::Borrowed {
                    path,
                    clips: borrowed,
                } => {
                    if adjustments
                        .0
                        .insert(pending.path.clone(), SwingSource::Twin)
                        .is_none()
                    {
                        info!(
                            door = format_args!("{:08X}", door_row.ref_id),
                            model = %pending.path,
                            twin = %path,
                            degrees = borrowed.swing,
                            "load door borrows its non-load twin's swing"
                        );
                    }
                    Some(*borrowed)
                }
            }
        };

        // Whichever swing the door ended up with, it is the door's animation from here on: the same
        // state machine and the same leaf hiding, told a clip that opens the doorway.
        let resolved = match borrowed {
            Some(borrowed) => ResolvedModel {
                player: resolved.player,
                open: clips.add(borrowed.open),
                close: borrowed.close.map(|clip| clips.add(clip)),
                open_seconds: borrowed.seconds,
                open_swing: borrowed.swing,
                close_seconds: borrowed.close_seconds,
                moved: borrowed.moved,
            },
            // No twin, or one that is no use to this door: the door's own clip is the swing it has,
            // and the leaves in it are already turning the right way about the right hinges.
            None => swung_open(
                resolved,
                &mut clips,
                &mut adjustments,
                door_row.ref_id,
                &pending.path,
            ),
        };
        attach_animation(
            &mut commands,
            &mut graphs,
            door,
            resolved,
            &children,
            &targets,
        );
    }
}

/// What came of asking a load door's non-load twin for its swing.
#[derive(Debug)]
enum TwinSwing {
    /// The twin, or one of its clips, is still on its way: try again next frame.
    Waiting,
    /// The model's name carries no load marker, so there is no twin to look for.
    NoTwin,
    /// There is one and it is no use: it never loaded, or its clips move nodes this door's model has
    /// not got. Its path is carried for the log.
    Unusable(String),
    /// The twin's `Open`/`Close` rebuilt onto the door's own nodes, and the twin's path for the log.
    ///
    /// Boxed because the clips are large and one of this for a door is not worth carrying in every
    /// `Waiting` of every door that never gets one.
    Borrowed {
        path: String,
        clips: Box<BorrowedClips>,
    },
}

/// The swing a door's non-load twin has for it, if it has one the door can use.
///
/// The twin is looked up once - the same model path with the load marker taken out of it, loaded
/// then and there - and from the next frame this is the question "has it arrived, and does it fit?".
/// [`TwinSwing::Waiting`] answers for as long as something is still on its way; a twin that never
/// arrives, and one whose clips move other nodes, are [`TwinSwing::Unusable`] and the door keeps its
/// own clip.
#[allow(clippy::too_many_arguments)]
fn twin_swing(
    pending: &mut PendingDoorModel,
    player: Entity,
    asset_server: &AssetServer,
    models: &Assets<Gltf>,
    clips: &Assets<AnimationClip>,
    children: &Query<&Children>,
    names: &Query<&Name>,
    targets: &Query<&AnimationTargetId>,
) -> TwinSwing {
    let Some(twin) = pending.twin.clone() else {
        let Some(path) = twin_model_path(&pending.path) else {
            return TwinSwing::NoTwin;
        };
        pending.waiting = 0;
        pending.twin = Some(TwinModel {
            model: asset_server.load(path.clone()),
            path,
        });
        return TwinSwing::Waiting;
    };

    let Some(twin_model) = models.get(&twin.model) else {
        // A twin whose load failed is not going to arrive, and one that has outlasted
        // [`TWIN_WAIT_FRAMES`] is not either.
        if asset_server.load_state(&twin.model).is_failed() {
            return TwinSwing::Unusable(twin.path);
        }
        pending.waiting += 1;
        return if pending.waiting < TWIN_WAIT_FRAMES {
            TwinSwing::Waiting
        } else {
            TwinSwing::Unusable(twin.path)
        };
    };

    // The twin's clips are named sub-assets of its model and can arrive a frame after it, the way
    // the door's own did - and they are the whole reason for waiting for a twin at all.
    let (open, close) = door_clips(twin_model);
    let clips_are_here = open.as_ref().is_some_and(|open| clips.get(open).is_some())
        && close
            .as_ref()
            .is_none_or(|close| clips.get(close).is_some());
    if !clips_are_here {
        pending.waiting += 1;
        return if pending.waiting < TWIN_WAIT_FRAMES {
            TwinSwing::Waiting
        } else {
            TwinSwing::Unusable(twin.path)
        };
    }

    match open.and_then(|open| {
        borrow_swing(
            player,
            &open,
            close.as_ref(),
            clips,
            children,
            names,
            targets,
        )
    }) {
        Some(clips) => TwinSwing::Borrowed {
            path: twin.path,
            clips: Box::new(clips),
        },
        None => TwinSwing::Unusable(twin.path),
    }
}

/// Gives a door its animation: a graph of the clips it is to play, the leaves marked, and the
/// [`DoorAnimation`] the state machine reads from then on.
fn attach_animation(
    commands: &mut Commands,
    graphs: &mut Assets<AnimationGraph>,
    door: Entity,
    resolved: ResolvedModel,
    children: &Query<&Children>,
    targets: &Query<&AnimationTargetId>,
) {
    let mut clip_handles = vec![resolved.open.clone()];
    if let Some(close) = &resolved.close {
        clip_handles.push(close.clone());
    }
    let (graph, nodes) = AnimationGraph::from_clips(clip_handles);
    let graph = graphs.add(graph);
    let has_close = resolved.close.is_some();

    mark_leaf_nodes(commands, door, &resolved.moved, children, targets);

    commands
        .entity(resolved.player)
        .try_insert((AnimationGraphHandle(graph), AnimationTransitions::new()));
    commands
        .entity(door)
        .try_insert(DoorAnimation {
            player: Some(resolved.player),
            open: Some(DoorClip {
                node: nodes[0],
                seconds: resolved.open_seconds,
            }),
            close: has_close.then(|| DoorClip {
                node: nodes[1],
                seconds: resolved.close_seconds.unwrap_or(0.0),
            }),
            clears_doorway: resolved.open_swing >= DOORWAY_CLEAR_DEGREES,
        })
        .try_remove::<PendingDoorModel>();
}

/// What a door model's clips come to: their lengths, how far the `Open` one turns the leaves, the
/// entity the loader put the door's [`AnimationPlayer`] on, and the nodes they move. `None` while
/// any of that is still on its way - the clip sub-assets are loaded after the model, and the scene
/// the player lives in is spawned after that.
fn resolve_model(
    door: Entity,
    open: &Handle<AnimationClip>,
    close: Option<&Handle<AnimationClip>>,
    clips: &Assets<AnimationClip>,
    children: &Query<&Children>,
    players: &Query<(), With<AnimationPlayer>>,
) -> Option<ResolvedModel> {
    let open_clip = clips.get(open)?;
    let close_clip = match close {
        Some(close) => Some(clips.get(close)?),
        None => None,
    };
    // The loader puts the door's player on the animation root of the scene, which it spawns under
    // the door reference a frame or more after the model itself is loaded.
    let player = std::iter::once(door)
        .chain(children.iter_descendants(door))
        .find(|entity| players.contains(*entity))?;
    let moved = clip_targets(std::iter::once(open_clip).chain(close_clip));
    Some(ResolvedModel {
        player,
        open: open.clone(),
        close: close.cloned(),
        open_seconds: open_clip.duration(),
        open_swing: swing_degrees(open_clip, &moved),
        close_seconds: close_clip.map(AnimationClip::duration),
        moved,
    })
}

/// The non-load twin's swing, rebuilt onto the door's own nodes - or `None` when the twin is not
/// the same door: a clip that moves a node this model has not got would swing nothing, and one that
/// moves only some of them would swing half a door, which is worse than a door that keeps its own
/// clip.
///
/// The clips are rebuilt rather than borrowed as they are because an [`AnimationTargetId`] is a
/// hash of a node's **whole** name path from the scene root (`bevy_gltf-0.19.0/src/loader/mod.rs`
/// `#L559`, `#L1558`) and the two models name their roots differently: `DwemerLargeDoorLoad01`
/// against `DwemerLargeDoor01`. Every id in the twin's clip therefore names a node the door's scene
/// does not have, and the door's `Object02` is not the twin's `Object02` either. Rebuilding under
/// the door's own ids is what makes the twin's curves drive the door's leaves.
fn borrow_swing(
    player: Entity,
    twin_open: &Handle<AnimationClip>,
    twin_close: Option<&Handle<AnimationClip>>,
    clips: &Assets<AnimationClip>,
    children: &Query<&Children>,
    names: &Query<&Name>,
    targets: &Query<&AnimationTargetId>,
) -> Option<BorrowedClips> {
    let open = clips.get(twin_open)?;
    let close = match twin_close {
        Some(close) => Some(clips.get(close)?),
        None => None,
    };
    // Everything the twin's clips move, and how each of those nodes is named in the door's own
    // model. Empty means the twin animates nothing, which is no swing to borrow.
    let wanted: HashSet<AnimationTargetId> = std::iter::once(open)
        .chain(close)
        .flat_map(|clip| clip.curves().keys().copied())
        .collect();
    let mapping = twin_node_mapping(&scene_node_paths(player, children, names, targets), &wanted);
    if wanted.is_empty() || mapping.len() != wanted.len() {
        return None;
    }

    let open = rebuild_clip(open, &mapping);
    let close = close.map(|close| rebuild_clip(close, &mapping));
    let moved: HashSet<AnimationTargetId> = mapping.values().copied().collect();
    let seconds = open.duration();
    let close_seconds = close.as_ref().map(AnimationClip::duration);
    let swing = swing_degrees(&open, &moved);
    Some(BorrowedClips {
        open,
        close,
        seconds,
        swing,
        close_seconds,
        moved,
    })
}

/// Where the twin's clips say each of the door's own nodes is: the twin's id for a node, against
/// the id the door's scene gives it.
///
/// A twin's clip can only be played on the door's nodes if both models call those nodes the same
/// thing, and the only way to ask "which of my nodes does this id name?" is to work out what the
/// paths of the twin's ids are. The two models are the same door - the load one keeping the leaves
/// still while the loading screen comes down - so the twin's path for a node is the door's path
/// with the model's own root name written the way the twin writes it
/// ([`twin_target_ids`]). An id that matches is proof the twin names that node: the id hashes the
/// whole path, so nothing but the same names can produce it.
fn twin_node_mapping(
    nodes: &[(AnimationTargetId, Vec<Name>)],
    wanted: &HashSet<AnimationTargetId>,
) -> HashMap<AnimationTargetId, AnimationTargetId> {
    let mut mapping = HashMap::new();
    for (door, path) in nodes {
        for twin in twin_target_ids(path) {
            if wanted.contains(&twin) {
                mapping.insert(twin, *door);
            }
        }
    }
    mapping
}

/// The name path from a door's animation root down to each node under it, with the node's own
/// [`AnimationTargetId`].
///
/// This is the path the loader hashed to make that id - the root's own name first
/// (`bevy_gltf-0.19.0/src/loader/mod.rs#L1545`) - rebuilt from the spawned scene, so that the same
/// path can be asked for under another model's root node name.
fn scene_node_paths(
    root: Entity,
    children: &Query<&Children>,
    names: &Query<&Name>,
    targets: &Query<&AnimationTargetId>,
) -> Vec<(AnimationTargetId, Vec<Name>)> {
    let mut nodes = Vec::new();
    let mut stack = vec![(root, Vec::new())];
    while let Some((entity, mut path)) = stack.pop() {
        let Ok(name) = names.get(entity) else {
            // The loader names every node it spawns; one that is not named is not a path the twin
            // could be asking about either.
            continue;
        };
        path.push(name.clone());
        if let Ok(target) = targets.get(entity) {
            nodes.push((*target, path.clone()));
        }
        if let Ok(kids) = children.get(entity) {
            stack.extend(kids.iter().map(|kid| (kid, path.clone())));
        }
    }
    nodes
}

/// The ids a twin's clips would use for a node the door's own model has at `path`.
///
/// The twin is the same model under a root name carrying no marker: for the demo route's doors,
/// `DwemerLargeDoorLoad01` here is `DwemerLargeDoor01` there. Taking the marker out of a
/// marker-carrying element of the path - the model's root node is named after the file the marker
/// was taken out of - turns the door's id into the twin's. Which element that is comes from the
/// path itself rather than from a rule about where a converter puts a model's root, and a path with
/// no marker in it yields nothing: a model whose names have nothing to do with its twin's is left
/// alone.
fn twin_target_ids(path: &[Name]) -> Vec<AnimationTargetId> {
    let mut ids = Vec::new();
    for index in 0..path.len() {
        let Some(stripped) = strip_load_marker(path[index].as_str()) else {
            continue;
        };
        let mut twin_path = path.to_vec();
        twin_path[index] = Name::new(stripped);
        ids.push(AnimationTargetId::from_names(twin_path.iter()));
    }
    ids
}

/// One of the twin's clips with every curve it has for a node of the door's model moved to that
/// node's own id, and the curves for anything else dropped - there are none, `mapping` covers the
/// clip exactly.
fn rebuild_clip(
    twin: &AnimationClip,
    mapping: &HashMap<AnimationTargetId, AnimationTargetId>,
) -> AnimationClip {
    let mut rebuilt = AnimationClip::default();
    for (target, curves) in twin.curves() {
        let Some(door) = mapping.get(target) else {
            continue;
        };
        for curve in curves {
            rebuilt.add_variable_curve_to_target(*door, curve.clone());
        }
    }
    // The same length as the clip it came from, whatever its curves say: this is the denominator of
    // `OPEN_FRACTION` and the time `Close` has to reach.
    rebuilt.set_duration(twin.duration());
    rebuilt
}

/// Gives a door whose own `Open` clip does not clear the doorway a swing of its own, by turning
/// that clip further than it was baked to turn.
///
/// This is the answer for the load doors no twin can help - which is very nearly all of them: a load
/// door's clip is short because the game only had to move the leaf before the loading screen covered
/// it, and the two models of a load/twin pair name their leaves differently as often as not. The
/// door's own clip already has everything a swing needs except the distance - the leaves, the hinge
/// each one turns about, the direction and the timing - and [`swung_open_clips`] stretches it until
/// its widest leaf stands at [`SWUNG_OPEN_DEGREES`].
///
/// The door comes back unchanged when there is nothing to scale (a clip that turns nothing, which
/// has no rotation to stretch, and one that already clears the doorway, which is never scaled down)
/// or when its clips cannot be rebuilt; its leaves are then hidden when it opens, as before.
fn swung_open(
    resolved: ResolvedModel,
    clips: &mut Assets<AnimationClip>,
    adjustments: &mut AdjustedSwings,
    ref_id: u32,
    model_path: &str,
) -> ResolvedModel {
    let factor = swing_scale(resolved.open_swing);
    if factor <= 1.0 {
        return resolved;
    }
    let Some((open, close)) = swung_open_clips(
        clips.get(&resolved.open),
        resolved.close.as_ref().and_then(|close| clips.get(close)),
        &resolved.moved,
        factor,
    ) else {
        debug!(
            door = format_args!("{ref_id:08X}"),
            model = %model_path,
            "the door's own clip could not be scaled; keeping it as it is"
        );
        return resolved;
    };

    let swing = swing_degrees(&open, &resolved.moved);
    let open_seconds = open.duration();
    let close_seconds = close.as_ref().map(AnimationClip::duration);
    if adjustments
        .0
        .insert(model_path.to_owned(), SwingSource::Scaled)
        .is_none()
    {
        info!(
            door = format_args!("{ref_id:08X}"),
            model = %model_path,
            from = resolved.open_swing,
            to = swing,
            factor,
            "load door's own swing scaled up to open the doorway"
        );
    }
    ResolvedModel {
        player: resolved.player,
        open: clips.add(open),
        close: close.map(|clip| clips.add(clip)),
        open_seconds,
        open_swing: swing,
        close_seconds,
        moved: resolved.moved,
    }
}

/// How much a door's own `Open` clip is scaled by, given how far it turns its widest leaf.
///
/// One factor for the whole clip, taken from the widest leaf, so that the leaves keep their motion
/// relative to each other: a door that turns one leaf 8 degrees and the other 12 comes out with the
/// same 2:3 between them.
///
/// A factor of 1 means "leave it alone", and two things get one: a clip that already opens the
/// doorway - scaling a door's swing *down* would be as wrong as scaling a narrow one up - and a clip
/// that turns nothing at all, which has no rotation to scale. The rest are scaled to
/// [`SWUNG_OPEN_DEGREES`], and capped at [`MAX_SWING_SCALE`].
fn swing_scale(swing_degrees: f32) -> f32 {
    if swing_degrees.is_nan() || swing_degrees <= 0.0 || swing_degrees >= DOORWAY_CLEAR_DEGREES {
        return 1.0;
    }
    (SWUNG_OPEN_DEGREES / swing_degrees).min(MAX_SWING_SCALE)
}

/// A door's own `Open` and `Close` with their swing scaled up until the doorway opens.
///
/// The nodes, the hinge each leaf turns about and the way it turns are all in the clip already; what
/// a load door's clip does not have is the distance, because the game only had to move the leaf
/// before the loading screen covered it. So every rotation curve is turned `factor` times as far
/// from the pose the clip starts on, about the same axis: `t = 0` is the pose the door stands in and
/// does not move, the clip keeps its own length, and its last key stands its widest leaf at about
/// [`SWUNG_OPEN_DEGREES`]. Nothing else in the clip is touched - a door that slides something while
/// it swings keeps sliding it exactly as far.
///
/// `Close` is the scaled `Open` **run backwards** when the door's own `Close` is its `Open`
/// backwards ([`close_mirrors_open`]) - the two sequences of one model, which is how every load door
/// in the install that has both is built - because closing is the door retracing the swing it just
/// made, and a `Close` scaled about its own first pose would instead carry the leaf *past* closed.
/// A `Close` that is a motion of its own is scaled the way `Open` is, about its own first pose.
///
/// `None` when there is no clip to scale or a curve cannot be rebuilt (times that are not a curve,
/// keys that are not finite), which leaves the door with the clip it has.
fn swung_open_clips(
    open: Option<&AnimationClip>,
    close: Option<&AnimationClip>,
    moved: &HashSet<AnimationTargetId>,
    factor: f32,
) -> Option<(AnimationClip, Option<AnimationClip>)> {
    let open = open?;
    let swung = rebuilt_clip(open, Resample::SwungOpen(factor))?;
    let close = match close {
        Some(close) if close_mirrors_open(open, close, moved) => {
            Some(rebuilt_clip(&swung, Resample::Reversed)?)
        }
        Some(close) => Some(rebuilt_clip(close, Resample::SwungOpen(factor))?),
        None => None,
    };
    Some((swung, close))
}

/// Whether a door's `Close` clip is its `Open` clip run backwards: where the `Open` clip ends is
/// where `Close` starts, and where `Open` starts is where `Close` ends, for every node the swing
/// turns to within [`CLOSE_MIRROR_DEGREES`].
///
/// This is what a door's two controller sequences are in the install - `Close` plays the `Open`
/// sequence's keys from the other end - and it is the difference between a door that closes by
/// retracing its swing and one whose `Close` is a second motion in its own right.
fn close_mirrors_open(
    open: &AnimationClip,
    close: &AnimationClip,
    moved: &HashSet<AnimationTargetId>,
) -> bool {
    // A `Close` that moves a node the `Open` does not is a motion of its own, whatever its poses
    // are: reversing the `Open` would leave that node standing.
    if close
        .curves()
        .keys()
        .any(|target| !open.curves().contains_key(target))
    {
        return false;
    }
    let rotation = animated_field!(Transform::rotation);
    let mut compared = 0;
    for target in moved {
        let Some(rest) = open.sample_clamped(rotation.clone(), *target, 0.0) else {
            continue;
        };
        let Some(swung) = open.sample_clamped(rotation.clone(), *target, open.duration()) else {
            continue;
        };
        let (Some(closed), Some(reopened)) = (
            close.sample_clamped(rotation.clone(), *target, 0.0),
            close.sample_clamped(rotation.clone(), *target, close.duration()),
        ) else {
            // The `Close` clip does not cover a node the swing turns: not the same motion.
            return false;
        };
        let within = |a: Quat, b: Quat| a.angle_between(b).to_degrees() <= CLOSE_MIRROR_DEGREES;
        if !within(closed, swung) || !within(reopened, rest) {
            return false;
        }
        compared += 1;
    }
    compared > 0
}

/// What a clip is being rebuilt for ([`rebuilt_clip`]).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Resample {
    /// Every rotation turned `factor` times as far from the pose the clip starts on
    /// ([`swung_open_clips`]).
    SwungOpen(f32),
    /// Every sample taken the same distance from the *other* end of the clip: the clip run
    /// backwards, which is how a door's scaled `Close` is made from its scaled `Open`.
    Reversed,
}

impl Resample {
    /// The time of the original clip a rebuilt sample at `time` comes from.
    fn source(self, time: f32, duration: f32) -> f32 {
        match self {
            Resample::SwungOpen(_) => time,
            Resample::Reversed => duration - time,
        }
    }

    /// The pose a rebuilt clip holds where the original reaches `pose`, given the pose the original
    /// starts on.
    ///
    /// The swing is a rotation about one axis, so turning it further is turning the same rotation
    /// further: the rotation from the rest pose to the pose, multiplied by `factor` about the same
    /// axis. `Quat::slerp` from the identity does exactly that, and is what keeps a leaf's hinge and
    /// direction - and the leaves' motion relative to each other - exactly as the clip had them. A
    /// pose that *is* the rest pose stays there whatever the factor is, which is what makes `t = 0`
    /// the one sample a scaled clip shares with the clip it came from.
    fn rotation(self, rest: Quat, pose: Quat) -> Quat {
        match self {
            Resample::SwungOpen(factor) => {
                rest * Quat::slerp(Quat::IDENTITY, rest.inverse() * pose, factor)
            }
            Resample::Reversed => pose,
        }
    }
}

/// One of a door's own clips rebuilt sample by sample ([`Resample`]): every rotation, translation
/// and scale curve it has, at [`sample_times`] from its start to its end. Anything else a clip
/// carries is copied over untouched.
///
/// Sampling rather than reading the clip's own keys is what lets a curve be turned about the pose it
/// starts on and run backwards, and the times are the same for every curve, so the rebuilt clip
/// keeps the shape of the motion. `None` when a rotation curve cannot be sampled, or when the
/// samples are not a curve at all (times that do not increase, values that are not finite) - the
/// caller's cue to leave the door with the clip it has rather than half a swing.
fn rebuilt_clip(clip: &AnimationClip, resample: Resample) -> Option<AnimationClip> {
    let duration = clip.duration();
    let times = sample_times(duration);
    let rotation = animated_field!(Transform::rotation);
    let translation = animated_field!(Transform::translation);
    let scale = animated_field!(Transform::scale);
    let rotation_id = rotation.evaluator_id();
    let translation_id = translation.evaluator_id();
    let scale_id = scale.evaluator_id();

    let mut rebuilt = AnimationClip::default();
    for (target, curves) in clip.curves() {
        for curve in curves {
            let field = curve.0.evaluator_id();
            if field == rotation_id {
                let rest = clip.sample_clamped(rotation.clone(), *target, 0.0)?;
                let mut samples = resampled(clip, rotation.clone(), *target, &times, resample)?;
                for (_, pose) in &mut samples {
                    *pose = resample.rotation(rest, *pose);
                }
                let samples = AnimatableKeyframeCurve::new(samples).ok()?;
                rebuilt
                    .add_curve_to_target(*target, AnimatableCurve::new(rotation.clone(), samples));
            } else if field == translation_id {
                let samples = resampled(clip, translation.clone(), *target, &times, resample)?;
                let samples = AnimatableKeyframeCurve::new(samples).ok()?;
                rebuilt.add_curve_to_target(
                    *target,
                    AnimatableCurve::new(translation.clone(), samples),
                );
            } else if field == scale_id {
                let samples = resampled(clip, scale.clone(), *target, &times, resample)?;
                let samples = AnimatableKeyframeCurve::new(samples).ok()?;
                rebuilt.add_curve_to_target(*target, AnimatableCurve::new(scale.clone(), samples));
            } else {
                // Not a field this knows how to rebuild: a clip's other curves are not the swing,
                // so they are carried over as they are.
                rebuilt.add_variable_curve_to_target(*target, curve.clone());
            }
        }
    }
    rebuilt.set_duration(duration);
    Some(rebuilt)
}

/// One animatable field of one node, sampled at the rebuilt clip's times, from wherever
/// [`Resample`] says to read the original.
fn resampled<A: Animatable>(
    clip: &AnimationClip,
    property: impl AnimatableProperty<Property = A> + Clone,
    target: AnimationTargetId,
    times: &[f32],
    resample: Resample,
) -> Option<Vec<(f32, A)>> {
    let duration = clip.duration();
    let mut samples = Vec::with_capacity(times.len());
    for time in times {
        let value =
            clip.sample_clamped(property.clone(), target, resample.source(*time, duration))?;
        samples.push((*time, value));
    }
    Some(samples)
}

/// The times a rebuilt clip is sampled at, from its start to its end, both included: sixty to the
/// second of the clip's own length, never fewer than [`MIN_SWING_SAMPLES`] nor more than
/// [`MAX_SWING_SAMPLES`].
///
/// The door's clip is a baked sequence of linear keys at a handful of times, and a door swing is a
/// short smooth motion, so a sample a frame or so apart keeps the shape of it. Both ends are
/// included because both are load-bearing: the first sample is the pose the door stands in (and is
/// the one a scaled clip leaves exactly where it was), and the last is the pose a scaled clip opens
/// the door to.
fn sample_times(duration: f32) -> Vec<f32> {
    let wanted = (duration * SWING_SAMPLES_PER_SECOND).ceil();
    let count = if wanted.is_finite() {
        (wanted as u32).clamp(MIN_SWING_SAMPLES, MAX_SWING_SAMPLES)
    } else {
        MIN_SWING_SAMPLES
    };
    let last = (count - 1) as f32;
    (0..count)
        .map(|sample| duration * sample as f32 / last)
        .collect()
}

/// How far a door's `Open` clip turns its leaves, in degrees: the widest angle any node the clip
/// moves sweeps from the clip's first key to its last.
///
/// The clip knows the pose it starts from and the pose it ends on, and those two are what decide
/// whether the doorway ends up open: a leaf that ends 8 degrees from where it started has moved
/// about as far as its own thickness, and the doorway behind it is still a wall. Read here rather
/// than watched at runtime, because the answer is a fact about the clip - and because a door's
/// leaves must not be hidden partway through a swing that is going to clear the opening.
///
/// A clip with no rotation curves at all - a door that slides, or one that only moves something
/// that is not the leaf - turns nothing, so it does not clear.
fn swing_degrees(clip: &AnimationClip, moved: &HashSet<AnimationTargetId>) -> f32 {
    let rotation = |target: AnimationTargetId, time: f32| {
        clip.sample_clamped(animated_field!(Transform::rotation), target, time)
    };
    let mut widest = 0.0_f32;
    for target in moved {
        let (Some(start), Some(end)) = (rotation(*target, 0.0), rotation(*target, clip.duration()))
        else {
            continue;
        };
        let degrees = start.angle_between(end).to_degrees();
        if degrees.is_finite() {
            widest = widest.max(degrees);
        }
    }
    widest
}

/// `E` on a load door, or a script's request: starts the door's own `Open` clip, or - for a door
/// whose model has no clip, or whose clips have not arrived yet - promotes it to
/// `Open { animated: false }` in this same frame, which is what a door did before there were clips.
///
/// The message is [`OpenDoor`], which is what the player's `E` writes. [`ActivateDoor`] is not read
/// here on purpose: that is a scripted run's crossing, it moves the camera in the same frame, and
/// starting a swing for a door the camera has already left is work nobody sees
/// ([`crate::doors::ActivateDoor`]).
///
/// A second activation while the swing is in flight does nothing: a door that is already opening
/// cannot be told anything new, and one that is closing is on its way back to the rest pose, which
/// is the only pose a `Close` clip is allowed to run from.
fn activate_doors(
    mut requests: MessageReader<OpenDoor>,
    mut doors: Query<(&LoadDoor, &mut DoorState, Option<&DoorAnimation>)>,
    mut players: Query<(&mut AnimationPlayer, Option<&mut AnimationTransitions>)>,
) {
    for request in requests.read() {
        let Ok((door, mut state, animation)) = doors.get_mut(request.door) else {
            continue;
        };
        // An invisible `AutoLoadDoor01` marker has no leaf to open: `player_auto_doors` crosses it
        // on contact, and the portal never draws it.
        if door.auto_load {
            continue;
        }
        let animation = animation.copied().unwrap_or_default();
        match *state {
            DoorState::Closed => {
                // The rest pose is the pose the `Open` clip starts from, so the swing begins at
                // zero.
                *state = if play_clip(&mut players, animation, animation.open, 0.0) {
                    DoorState::Opening
                } else {
                    // Nothing to play - a static leaf, or a model whose clips are still loading.
                    // The doorway opens in this frame.
                    DoorState::Open { animated: false }
                };
            }
            DoorState::Open { .. } => {
                if let Some(close) = animation.close {
                    // The door may still be swinging when it is told to close: `Open` is reached
                    // halfway through the clip, and the clip plays on to its end. Starting `Close`
                    // at the pose the `Open` clip has reached makes the reversal continuous
                    // whatever the two sequences' timings are, and the fade covers the rest.
                    let reached = clip_fraction_of(&players, animation, animation.open);
                    let seek = (1.0 - reached) * close.seconds;
                    if play_clip(&mut players, animation, Some(close), seek) {
                        *state = DoorState::Closing;
                    }
                } else if play_clip(&mut players, animation, animation.open, 0.0) {
                    // A model with an `Open` clip and no `Close`: re-activation replays the open,
                    // and the door never closes (design section 5). There is no pose to match - a
                    // replay starts at the rest pose - so this is the one reversal that moves.
                    *state = DoorState::Opening;
                }
            }
            DoorState::Opening | DoorState::Closing => {}
        }
    }
}

/// How many frames a crossing's far door may be waited for before the opening is dropped.
///
/// The destination is streamed in before a crossing is applied
/// ([`crate::transition::destination_is_ready`]), so the far door is normally spawned frames before
/// the player walks through the source doorway; the wait is the backstop for the door that never
/// turns up (a link whose reference is not in the world at all). Giving up is silent: there is
/// nothing to do about it here, and the door is left exactly as it was.
const ARRIVAL_OPEN_WAIT_FRAMES: u32 = 60;

/// The far doors of crossings that have landed but could not be opened yet: normally empty, one
/// entry for a door that is still to be spawned.
#[derive(Resource, Default)]
struct ArrivalOpenings {
    /// One per crossing waiting for its far door, oldest first.
    openings: Vec<PendingArrivalOpen>,
}

/// A far door a crossing landed through, waiting to be opened: which reference it is, the point of
/// its `Open` clip the player arrived at, and how long it has been waiting.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PendingArrivalOpen {
    /// `door_links.destination_ref_id` of the link the crossing was made through: the reference that
    /// has to be got out of the player's way.
    destination_ref_id: u32,
    /// How far through the far door's own `Open` clip it is put when it is found
    /// ([`reached_fraction`], or the end of the clip when the source door's own swing cannot be
    /// asked).
    fraction: f32,
    /// Frames this has waited for its door, against [`ARRIVAL_OPEN_WAIT_FRAMES`].
    frames: u32,
}

/// Opens the far door of a crossing in the frame the player arrives through it.
///
/// A mapped crossing of an anchored door lands the player **in the destination doorway's own
/// plane** (`crate::transition`), which is where the far door of the link stands. Through the source
/// doorway that door was out of the way: the portal hid its leaf while the quad stood in its
/// doorway, and the source door's own leaves were mirrored onto the destination doorway open. So the
/// frame of the swap has to leave the far door out of the way too, or the first thing the player
/// sees in the new space is the back of a closed leaf - at point-blank range - and the walk out of
/// the doorway is blocked by it.
///
/// The pose is the window's: the far door's own `Open` clip, started at the point the source door's
/// own clip has reached ([`reached_fraction`]), so a door that was half open in the window is half
/// open in the room and a door that was fully open is fully open - no swing through the player
/// either way. A door the far side cannot ask - no animation of its own, no clip, a player that is
/// gone - is opened **as far as its clip goes**, which [`open_arrival_door`] does by starting the
/// clip at its end. A door with nothing to animate at all is [`DoorState::Open`] with `animated`
/// false, which the portal already draws as a hole
/// ([`DoorState::hides_whole_reference`]).
///
/// A door that is already open ([`DoorState::is_open`]) is left where it is: whatever opened it, the
/// window showed the player a doorway and the doorway is what they walked into.
///
/// This is not an [open in the sense of `E`](OpenDoor): it never runs a swing from the rest pose and
/// it never closes anything. Nothing here opens a door on the *near* side of a crossing, and the far
/// door stays open afterwards until something else closes it (`crate::transition`'s design note: the
/// portal then renders back through it at the player's back, which is the doorway being a doorway).
fn open_arrival_doors(
    mut requests: MessageReader<OpenDestinationDoor>,
    mut pending: ResMut<ArrivalOpenings>,
    mut doors: Query<(Entity, &LoadDoor, &mut DoorState, Option<&DoorAnimation>)>,
    mut players: Query<(&mut AnimationPlayer, Option<&mut AnimationTransitions>)>,
) {
    for request in requests.read() {
        // The pose the window showed: the source door's own clip, or the far door's clip end when it
        // cannot be asked. Read here, in the frame of the crossing, while the door that carries it
        // is certainly still there.
        let fraction = doors
            .get(request.door)
            .ok()
            .and_then(|(_, _, _, animation)| animation)
            .and_then(|animation| reached_fraction(animation, &players))
            .unwrap_or(1.0);
        pending.openings.push(PendingArrivalOpen {
            destination_ref_id: request.destination_ref_id,
            fraction,
            frames: 0,
        });
    }

    pending.openings.retain_mut(|opening| {
        opening.frames += 1;
        if opening.frames > ARRIVAL_OPEN_WAIT_FRAMES {
            // The door never turned up: silently leave it as it is (there is nothing else to do).
            return false;
        }
        for (_, row, mut state, animation) in &mut doors {
            if row.ref_id != opening.destination_ref_id {
                continue;
            }
            // The far door of the *link*, which is the one `crate::portal::update_portal` finds the
            // same way: the reference the crossing's link names. A load door's reference is spawned
            // once, wherever in the resident grid its cell is.
            if open_arrival_door(&mut state, animation, &mut players, opening.fraction) {
                info!(
                    door = format_args!("{:08X}", row.ref_id),
                    fraction = opening.fraction,
                    state = ?*state,
                    "arrival: opened the far door of a crossing"
                );
            }
            return false;
        }
        // Not spawned yet: try again next frame.
        true
    });
}

/// Puts a far door in the state a crossing arrives at: `fraction` of the way through its own `Open`
/// clip, or an open door with nothing left to animate.
///
/// The clip is *started at* that point rather than played from the rest pose, so the leaf is where
/// the window showed it from the first frame and never swings through the player. The state follows
/// the pose the clip is at ([`OPEN_FRACTION`] is where a swing counts as an open doorway), so the
/// leaves of a clip that did not clear the opening are hidden by the frame's
/// [`update_door_leaves`] and the doorway is walkable either way. A door with nothing to play - a
/// static leaf, or a model whose clips are still loading - opens in this frame exactly as it does
/// when `E` opens it, which the portal draws as a hole.
///
/// Returns whether the door was opened by this call: a door that was already open is left where it
/// is, and says so by answering false.
fn open_arrival_door(
    state: &mut DoorState,
    animation: Option<&DoorAnimation>,
    players: &mut Query<(&mut AnimationPlayer, Option<&mut AnimationTransitions>)>,
    fraction: f32,
) -> bool {
    if state.is_open() {
        return false;
    }
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let played = animation.is_some_and(|animation| {
        let seek = fraction * animation.open.map_or(0.0, |open| open.seconds);
        play_clip(players, *animation, animation.open, seek)
    });
    *state = if !played {
        // Nothing to play - a static leaf, or a model whose clips are still loading: the doorway
        // opens in this frame, which is what the portal draws as a hole.
        DoorState::Open { animated: false }
    } else if fraction >= OPEN_FRACTION {
        DoorState::Open { animated: true }
    } else {
        // Still swinging: `advance_door_states` moves it on at [`OPEN_FRACTION`] like any other
        // swing, and until then the doorway is one a player may walk through ([`DoorState::is_open`]).
        DoorState::Opening
    };
    true
}

/// How far through its own `Open` clip a door's swing has got, or `None` when it cannot be asked: a
/// door with no animation of its own, no `Open` clip, no player where the animation says one is, or
/// a clip that is not playing at all. A finished clip counts as the whole way through
/// ([`clip_fraction`]), which is the ordinary case: the source doorway's door has been standing open
/// since the player walked up to it.
fn reached_fraction(
    animation: &DoorAnimation,
    players: &Query<(&mut AnimationPlayer, Option<&mut AnimationTransitions>)>,
) -> Option<f32> {
    let (player, open) = (animation.player?, animation.open?);
    let (player, _) = players.get(player).ok()?;
    clip_fraction(player, open)
}

/// Moves a door on when its clip reaches the point that matters: `Opening` becomes [`DoorState::Open`]
/// at [`OPEN_FRACTION`] of the `Open` clip, and `Closing` becomes [`DoorState::Closed`] when the
/// `Close` clip finishes.
///
/// The clip's own clock decides both, not a timer of this module's: `advance_animations` is the one
/// place that adds time to an animation, so a long frame moves the door on by exactly the clip time
/// that frame played, and a clip that is finished holds its last key - the door standing open -
/// without the state having to notice that it stopped.
fn advance_door_states(
    mut doors: Query<(&mut DoorState, &DoorAnimation)>,
    players: Query<&AnimationPlayer>,
) {
    for (mut state, animation) in &mut doors {
        let Some(player) = animation.player.and_then(|player| players.get(player).ok()) else {
            continue;
        };
        match *state {
            DoorState::Opening => {
                if let Some(open) = animation.open
                    && clip_fraction(player, open).is_some_and(|f| f >= OPEN_FRACTION)
                {
                    *state = DoorState::Open { animated: true };
                }
            }
            DoorState::Closing => {
                if let Some(close) = animation.close
                    && clip_fraction(player, close).is_some_and(|f| f >= 1.0)
                {
                    *state = DoorState::Closed;
                }
            }
            DoorState::Closed | DoorState::Open { .. } => {}
        }
    }
}

/// Draws or hides a door's leaves for the state the door is in.
///
/// The swing is what the player asked to see, so the leaves are drawn the whole time the `Open`
/// clip plays - `Closed`, `Opening` and `Closing` all draw them - and the only frame that changes
/// that is the one where the door becomes `Open`, where the answer is whether the clip left the
/// leaves in the opening ([`DoorAnimation::clears_doorway`]).
///
/// The write goes straight to the component rather than through `Commands`: a crossing despawns a
/// whole cell's worth of doors in the frame it happens in, and a command queued against a leaf the
/// same frame despawns is applied to a dead entity, which Bevy treats as a panic (the same note as
/// `portal::show_load_door_leaves`). Writing only on a change keeps the visibility hierarchy from
/// being recomputed for every leaf every frame.
fn update_door_leaves(
    doors: Query<(&DoorState, Option<&DoorAnimation>)>,
    held: Query<(), With<CrossingHeld>>,
    mut leaves: Query<(&DoorLeaf, &mut Visibility)>,
) {
    for (leaf, mut visibility) in &mut leaves {
        let Ok((state, animation)) = doors.get(leaf.door) else {
            // The door is gone (its cell unloaded): its leaves are going with it.
            continue;
        };
        let wanted = if leaves_are_drawn(*state, animation, held.contains(leaf.door)) {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
}

/// Whether a door's leaves are drawn in this state, for a door whose animation resolved to this and
/// which is (or is not) holding a crossing of its own.
fn leaves_are_drawn(state: DoorState, animation: Option<&DoorAnimation>, held: bool) -> bool {
    if held {
        // The crossing is waiting for its destination to stream in: the door is drawn as it was -
        // leaves and all - because there is no window through the doorway yet either, and a
        // doorway with neither is a hole in the world (`crate::transition::CrossingHeld`).
        return true;
    }
    match state {
        // A door that is `Open` and has an animation of its own shows its leaves only if the swing
        // took them out of the opening; one that turned its leaves a few degrees is a wall with a
        // wall behind it, so its leaves go.
        DoorState::Open { animated: true } => {
            animation.is_none_or(|animation| animation.clears_doorway)
        }
        // `Opening` and `Closing` are the swing itself, and a `Closed` door is a door: the leaves
        // are drawn. `Open { animated: false }` is a door with no leaf of its own to draw - and
        // its whole model is the portal's to hide, from `DoorState::hides_whole_reference`.
        DoorState::Closed
        | DoorState::Opening
        | DoorState::Closing
        | DoorState::Open { animated: false } => true,
    }
}

/// Starts one of a door's clips on its own player, `seek_time` seconds in, cross-fading from
/// whatever the door was playing.
///
/// The fade (design section 2.3) is what keeps a reversal from snapping: the clip that is being
/// left is faded out over [`CLIP_FADE`] rather than stopped dead, so a door told to close halfway
/// through opening continues from where it is rather than jumping to the pose `Close` happens to
/// start on. Both clips drive the same node transforms, so an un-faded pair would fight over the
/// pose at weight 1.0 each.
///
/// Returns false when there is nothing to play: no such clip, or no player (`None` for a model with
/// no clips, and a query miss for one whose cell unloaded in this same frame).
fn play_clip(
    players: &mut Query<(&mut AnimationPlayer, Option<&mut AnimationTransitions>)>,
    animation: DoorAnimation,
    start: Option<DoorClip>,
    seek_time: f32,
) -> bool {
    let (Some(player), Some(start)) = (animation.player, start) else {
        return false;
    };
    let Ok((mut player, transitions)) = players.get_mut(player) else {
        return false;
    };
    let seek_time = if seek_time.is_finite() {
        seek_time.clamp(0.0, start.seconds.max(0.0))
    } else {
        0.0
    };
    match transitions {
        Some(mut transitions) => {
            transitions
                .play(&mut player, start.node, CLIP_FADE)
                // `set_seek_time` rather than `seek_to`: the part of the clip that is being skipped
                // should not fire its events, and a door that is closing again should not replay
                // the sound of opening.
                .set_seek_time(seek_time);
        }
        // A player without the transitions component - a hand-built one - still plays the clip, it
        // just switches without a fade. `attach_door_animations` always inserts both together.
        None => {
            let playing = player.play(start.node);
            playing.replay();
            playing.set_seek_time(seek_time);
        }
    }
    true
}

/// How far through its clip a door's playing animation is, as a fraction of the clip's length, or
/// `None` while that node is not playing at all.
///
/// The clock is the one the pose is drawn from: `animate_targets` evaluates every curve at the
/// animation's `seek_time` (`bevy_animation-0.19.0/src/lib.rs#L1235`), while its `elapsed` is the
/// time the animation has been *playing*. The two agree on a clip that was started at its own zero,
/// which is every door the player opened, and they part company the moment anything seeks one -
/// which the reversal (`activate_doors`, a door told to close mid-swing) and a crossing's arrival
/// ([`open_arrival_door`], which puts the far door at the pose the window showed) both do. A state
/// read off the wrong clock says `Opening` at a pose that has already passed the mark, and that is a
/// leaf drawn in a doorway the player is walking through.
///
/// A finished clip counts as the whole way through, and so does a clip of no length: a model that
/// exported a zero-second sequence would otherwise leave its door mid-swing for good.
fn clip_fraction(player: &AnimationPlayer, clip: DoorClip) -> Option<f32> {
    let animation = player.animation(clip.node)?;
    if animation.is_finished() || clip.seconds <= 0.0 {
        return Some(1.0);
    }
    Some(animation.seek_time() / clip.seconds)
}

/// The same fraction for a clip whose animation has to be looked up through the door's player
/// first, which the activation path needs when it has to line a reversal up with the pose the door
/// has reached. Zero when there is no player to ask or no clip playing on it.
fn clip_fraction_of(
    players: &Query<(&mut AnimationPlayer, Option<&mut AnimationTransitions>)>,
    animation: DoorAnimation,
    clip: Option<DoorClip>,
) -> f32 {
    let (Some(player), Some(clip)) = (animation.player, clip) else {
        return 0.0;
    };
    let Ok((player, _)) = players.get(player) else {
        return 0.0;
    };
    clip_fraction(player, clip).unwrap_or(0.0)
}

/// The `Open` and `Close` clips a door model carries, by case-insensitive name.
///
/// `Open`/`Close` is a convention rather than a guarantee, so a model that names neither gets its
/// first clip as the opening one, and a model with no clips at all gets none - it is a static door.
fn door_clips(model: &Gltf) -> (Option<Handle<AnimationClip>>, Option<Handle<AnimationClip>>) {
    let named = |wanted: &str| {
        model
            .named_animations
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, clip)| clip.clone())
    };
    (
        named(OPEN_CLIP).or_else(|| model.animations.first().cloned()),
        named(CLOSE_CLIP),
    )
}

/// The animation targets a door's clips have curves for: exactly the nodes they move.
///
/// An [`AnimationTargetId`] on its own does not say which nodes those are, because `bevy_gltf` gives
/// one to every node under the animation root - the frame and the arch with it
/// (`bevy_gltf-0.19.0/src/loader/mod.rs#L1558`) - so the leaf is the intersection of the ids the
/// scene carries and the ids the clips name.
fn clip_targets<'a>(
    clips: impl IntoIterator<Item = &'a AnimationClip>,
) -> HashSet<AnimationTargetId> {
    clips
        .into_iter()
        .flat_map(|clip| clip.curves().keys().copied())
        .collect()
}

/// Marks the nodes a door's clips move with [`DoorLeaf`], so the walk probe can tell the moving
/// leaf from the frame it hangs in.
fn mark_leaf_nodes(
    commands: &mut Commands,
    door: Entity,
    moved: &HashSet<AnimationTargetId>,
    children: &Query<&Children>,
    targets: &Query<&AnimationTargetId>,
) {
    if moved.is_empty() {
        return;
    }
    for node in std::iter::once(door).chain(children.iter_descendants(door)) {
        if let Ok(target) = targets.get(node)
            && moved.contains(target)
        {
            commands.entity(node).try_insert(DoorLeaf { door });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        doors::{DoorCrossed, DoorDestination},
        player::{Player, eye_from_feet, player_walks_through_doors},
        profiling::ProfilingState,
        transition::CrossDoor,
        world::components::StreamingCamera,
    };
    use bevy::{
        animation::{
            AnimatedBy, animated_field,
            animation_curves::{AnimatableCurve, AnimatableKeyframeCurve},
        },
        asset::AssetApp,
        ecs::system::SystemState,
        time::TimeUpdateStrategy,
    };
    use std::time::Duration;

    /// How long the door's clips last in these tests.
    const CLIP_SECONDS: f32 = 1.0;

    /// The clip time one `app.update()` plays with [`TimeUpdateStrategy::ManualDuration`] - a tenth
    /// of a clip, so "0.4 s in" is four frames and the boundary at [`OPEN_FRACTION`] is between
    /// frames rather than on one.
    const STEP_SECONDS: f32 = 0.1;

    /// A leaf that swings clear of the doorway, like a Nordic or Imperial door (115-135 degrees).
    const WIDE_SWING_DEGREES: f32 = 120.0;

    /// A leaf that barely moves, like the demo route's `DweDoorLarge01Load` (5-9 degrees).
    const NARROW_SWING_DEGREES: f32 = 8.0;

    /// A model path with no `load` marker in it, and so no twin to look for.
    const PLAIN_MODEL_PATH: &str = "meshes/fixtures/door/door01.glb";

    /// A hand-built load door: the reference, the player and the scene nodes the loader would have
    /// spawned for a model whose `Open` clip moves one node.
    struct Door {
        door: Entity,
        player: Entity,
        /// The node the clips move - the leaf - with a mesh primitive under it.
        leaf: Entity,
        leaf_mesh: Entity,
        /// The door's frame: a node under the same animation root, carrying the same
        /// [`AnimationTargetId`] the loader gives every node there, but named by no clip.
        frame: Entity,
        frame_mesh: Entity,
        open_node: AnimationNodeIndex,
        close_node: AnimationNodeIndex,
    }

    /// A door with a model's clips, before [`attach_door_animations`] has run over it.
    struct ModelDoor {
        door: Entity,
        player: Entity,
        /// The node the `Open` clip moves, and its mesh primitive.
        moved: Entity,
        moved_mesh: Entity,
        /// A node under the animation root that no clip names.
        frame: Entity,
    }

    /// A door app with no window, no renderer and no GPU: the two animation asset collections,
    /// Bevy's own animation ticking, and a clock the test moves by [`STEP_SECONDS`] a frame.
    fn door_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::animation::AnimationPlugin,
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            STEP_SECONDS,
        )))
        .init_asset::<Gltf>()
        .add_plugins(DoorAnimationPlugin);
        app
    }

    /// The `LoadDoor` of a reference with a destination, so the door is one the crossing would use.
    fn load_door(auto_load: bool) -> LoadDoor {
        LoadDoor {
            ref_id: 0x0009_2809,
            destination: DoorDestination {
                destination_ref_id: 0x0006_998D,
                interior_cell_id: Some(7),
                worldspace_id: None,
                arrival_position: [0.0; 3],
                arrival_rotation: [0.0; 3],
            },
            label: "Alftand Great Lift".into(),
            auto_load,
            outward: Some([1.0, 0.0, 0.0]),
        }
    }

    /// A door whose leaf swings well clear of the doorway: the state machine does not care how far
    /// it moves, so this is the fixture for everything but the leaf-visibility tests.
    fn animated_door(app: &mut App) -> Door {
        animated_door_swinging(app, WIDE_SWING_DEGREES)
    }

    /// A door whose model has both clips, with the player, the graph, the transitions and the
    /// moving node the loader and [`attach_door_animations`] would have left behind: the `Open`
    /// clip turns the leaf `swing_degrees` over [`CLIP_SECONDS`], the `Close` clip turns it back,
    /// and the leaf's transform is there for `animate_targets` to write the pose into.
    fn animated_door_swinging(app: &mut App, swing_degrees: f32) -> Door {
        let leaf_target = AnimationTargetId::from_name(&Name::new("Object02"));
        let open = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip_swinging(CLIP_SECONDS, leaf_target, 0.0, swing_degrees));
        let close = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip_swinging(CLIP_SECONDS, leaf_target, swing_degrees, 0.0));
        let (graph, nodes) = AnimationGraph::from_clips([open, close]);
        let graph = app
            .world_mut()
            .resource_mut::<Assets<AnimationGraph>>()
            .add(graph);

        let door = app
            .world_mut()
            .spawn((load_door(false), DoorState::Closed))
            .id();
        let player = app
            .world_mut()
            .spawn((
                Name::new("DwemerLargeDoorLoad01"),
                AnimationPlayer::default(),
                AnimationGraphHandle(graph.clone()),
                AnimationTransitions::new(),
                ChildOf(door),
            ))
            .id();
        let leaf = app
            .world_mut()
            .spawn((
                Name::new("Object02"),
                leaf_target,
                AnimatedBy(player),
                Transform::default(),
                Visibility::default(),
                DoorLeaf { door },
                ChildOf(player),
            ))
            .id();
        let leaf_mesh = app.world_mut().spawn(ChildOf(leaf)).id();
        // The frame: under the animation root, so the loader gives it an `AnimationTargetId` too,
        // but no clip names it - which is what keeps it out of the leaf marking.
        let frame = app
            .world_mut()
            .spawn((
                Name::new("Plane04"),
                AnimationTargetId::from_name(&Name::new("Plane04")),
                ChildOf(player),
            ))
            .id();
        let frame_mesh = app.world_mut().spawn(ChildOf(frame)).id();

        app.world_mut().entity_mut(door).insert(DoorAnimation {
            player: Some(player),
            open: Some(DoorClip {
                node: nodes[0],
                seconds: CLIP_SECONDS,
            }),
            close: Some(DoorClip {
                node: nodes[1],
                seconds: CLIP_SECONDS,
            }),
            clears_doorway: swing_degrees >= DOORWAY_CLEAR_DEGREES,
        });
        Door {
            door,
            player,
            leaf,
            leaf_mesh,
            frame,
            frame_mesh,
            open_node: nodes[0],
            close_node: nodes[1],
        }
    }

    /// A door whose model has no clips: its state and its resolution are all it has.
    fn static_door(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((
                load_door(false),
                DoorState::Closed,
                DoorAnimation::default(),
            ))
            .id()
    }

    /// A clip of `seconds`, with no curves: enough for the state machine, which reads its length and
    /// the player's own clock.
    fn clip(seconds: f32) -> AnimationClip {
        let mut clip = AnimationClip::default();
        clip.set_duration(seconds);
        clip
    }

    /// A clip of `seconds` that turns `target` from `from_degrees` to `to_degrees` over its length,
    /// as the converter bakes a door's swing: one rotation curve on the node the clip moves.
    fn clip_swinging(
        seconds: f32,
        target: AnimationTargetId,
        from_degrees: f32,
        to_degrees: f32,
    ) -> AnimationClip {
        let mut clip = AnimationClip::default();
        clip.add_curve_to_target(
            target,
            AnimatableCurve::new(
                animated_field!(Transform::rotation),
                AnimatableKeyframeCurve::new([
                    (0.0, Quat::from_rotation_y(from_degrees.to_radians())),
                    (seconds, Quat::from_rotation_y(to_degrees.to_radians())),
                ])
                .expect("two keys at different times"),
            ),
        );
        clip
    }

    /// A door and the scene of a model whose `Open` clip turns one node by `swing_degrees`, spawned
    /// the way the loader spawns it: a player on the animation root, an `AnimationTargetId` on every
    /// node under it (the loader gives one to the whole animation-root subtree, frame included), and
    /// the moving node's mesh primitive under that.
    fn door_with_model(app: &mut App, swing_degrees: f32) -> ModelDoor {
        let moved_target = AnimationTargetId::from_name(&Name::new("Object02"));
        let open = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip_swinging(
                CLIP_SECONDS,
                moved_target,
                0.0,
                swing_degrees,
            ));
        let model = app.world_mut().resource_mut::<Assets<Gltf>>().add(gltf(
            std::slice::from_ref(&open),
            &[(OPEN_CLIP, open.clone())],
        ));

        let door = app
            .world_mut()
            .spawn((load_door(false), DoorState::Closed, pending(model)))
            .id();
        let player = app
            .world_mut()
            .spawn((
                Name::new("DwemerLargeDoorLoad01"),
                AnimationPlayer::default(),
                AnimationTargetId::from_name(&Name::new("DwemerLargeDoorLoad01")),
                ChildOf(door),
            ))
            .id();
        let moved = app
            .world_mut()
            .spawn((
                Name::new("Object02"),
                moved_target,
                AnimatedBy(player),
                Transform::default(),
                Visibility::default(),
                ChildOf(player),
            ))
            .id();
        let moved_mesh = app.world_mut().spawn(ChildOf(moved)).id();
        let frame = app
            .world_mut()
            .spawn((
                Name::new("Plane04"),
                AnimationTargetId::from_name(&Name::new("Plane04")),
                ChildOf(player),
            ))
            .id();
        ModelDoor {
            door,
            player,
            moved,
            moved_mesh,
            frame,
        }
    }

    /// A door waiting for its model, the way `request_door_models` leaves it - with no twin chosen
    /// yet, since a twin is only looked up once the door's own clip has turned out not to clear the
    /// doorway. The path decides whether there is one to look for: no `load` marker in it, no twin.
    fn pending(model: Handle<Gltf>) -> PendingDoorModel {
        PendingDoorModel {
            model,
            path: PLAIN_MODEL_PATH.to_owned(),
            twin: None,
            waiting: 0,
        }
    }

    /// A `Gltf` asset with no scenes, meshes or nodes - the test only needs its animations, which
    /// are the handles the loader names from the glTF document.
    fn gltf(clips: &[Handle<AnimationClip>], named: &[(&str, Handle<AnimationClip>)]) -> Gltf {
        Gltf {
            scenes: Vec::new(),
            named_scenes: Default::default(),
            meshes: Vec::new(),
            named_meshes: Default::default(),
            materials: Vec::new(),
            named_materials: Default::default(),
            nodes: Vec::new(),
            named_nodes: Default::default(),
            skins: Vec::new(),
            named_skins: Default::default(),
            default_scene: None,
            animations: clips.to_vec(),
            named_animations: named
                .iter()
                .map(|(name, clip)| (Box::from(*name), clip.clone()))
                .collect(),
            source: None,
        }
    }

    /// One frame with the `E` press for `door` in it, written before the frame runs the way the
    /// player's `E` and the demo tour's walk-through write it (the tour presses the key, the
    /// controller writes this message).
    fn activate(app: &mut App, door: Entity) {
        app.world_mut().write_message(OpenDoor { door });
        app.update();
    }

    /// `frames` frames, each of [`STEP_SECONDS`] of clip time.
    fn step(app: &mut App, frames: u32) {
        for _ in 0..frames {
            app.update();
        }
    }

    fn state(app: &App, door: Entity) -> DoorState {
        *app.world()
            .get::<DoorState>(door)
            .expect("a load door carries its state")
    }

    fn player(app: &App, door: Entity) -> &AnimationPlayer {
        app.world()
            .get::<AnimationPlayer>(door)
            .expect("the door's player")
    }

    /// How far the leaf is swung, in degrees, read off the transform `animate_targets` wrote for it:
    /// the yaw the fixture's clips turn it by.
    fn leaf_degrees(app: &App, leaf: Entity) -> f32 {
        let rotation = app
            .world()
            .get::<Transform>(leaf)
            .expect("the leaf's transform")
            .rotation;
        let (yaw, _, _) = rotation.to_euler(EulerRot::YXZ);
        yaw.to_degrees().abs()
    }

    fn leaf_visibility(app: &App, leaf: Entity) -> Visibility {
        *app.world()
            .get::<Visibility>(leaf)
            .expect("the leaf's visibility")
    }

    #[test]
    fn activating_a_closed_door_starts_it_opening() {
        let mut app = door_app();
        let door = animated_door(&mut app);
        assert_eq!(state(&app, door.door), DoorState::Closed);

        activate(&mut app, door.door);

        assert_eq!(state(&app, door.door), DoorState::Opening);
        assert!(
            player(&app, door.player).is_playing_animation(door.open_node),
            "the open clip is the one being played"
        );
        assert!(
            !player(&app, door.player).is_playing_animation(door.close_node),
            "and the close clip is not"
        );
    }

    #[test]
    fn the_door_is_open_once_the_clip_reaches_the_open_fraction() {
        let mut app = door_app();
        let door = animated_door(&mut app);
        activate(&mut app, door.door);

        // Four frames more: the clip is 0.4 s into its second, just short of the mark. (The clock
        // lives in `advance_animations`, which runs in `PostUpdate` after this module has looked -
        // and the app's first frame plays no clip time at all - so a frame lags the clip's own
        // time by one.)
        step(&mut app, 4);
        let elapsed = player(&app, door.player)
            .animation(door.open_node)
            .expect("the open clip is playing")
            .elapsed();
        assert!(
            elapsed < OPEN_FRACTION * CLIP_SECONDS,
            "{elapsed} is short of the mark"
        );
        assert_eq!(state(&app, door.door), DoorState::Opening);

        // Two more: the clip passes the mark, and is still playing - it runs to its end and holds
        // there.
        step(&mut app, 2);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        assert!(
            player(&app, door.player).is_playing_animation(door.open_node),
            "the open clip keeps playing past the mark"
        );
    }

    #[test]
    fn a_clip_that_ran_to_its_end_leaves_the_door_open() {
        let mut app = door_app();
        let door = animated_door(&mut app);
        activate(&mut app, door.door);
        step(&mut app, 12);

        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        assert!(
            player(&app, door.player)
                .animation(door.open_node)
                .unwrap()
                .is_finished(),
            "the clip has run out, holding its last key"
        );

        step(&mut app, 5);
        assert_eq!(
            state(&app, door.door),
            DoorState::Open { animated: true },
            "a finished clip does not close the door"
        );
    }

    #[test]
    fn a_door_with_no_clips_is_open_in_the_frame_it_is_activated() {
        let mut app = door_app();
        let door = static_door(&mut app);
        assert_eq!(state(&app, door), DoorState::Closed);

        activate(&mut app, door);

        assert_eq!(state(&app, door), DoorState::Open { animated: false });
    }

    // ---------------------------------------------------------------------------------------------
    // The far door of a crossing
    // ---------------------------------------------------------------------------------------------

    /// The reference of the door at the far end of `source`'s link: what
    /// [`OpenDestinationDoor`] names and the far door is found by.
    fn far_ref(app: &App, source: Entity) -> u32 {
        app.world()
            .get::<LoadDoor>(source)
            .expect("the source door's link")
            .destination
            .destination_ref_id
    }

    /// The far door of the link as the destination cell spawns it: the reference [`far_ref`] names.
    fn far_row(app: &App, source: Entity) -> LoadDoor {
        let mut row = load_door(false);
        row.ref_id = far_ref(app, source);
        row
    }

    /// The far door of the link, with a swing of its own.
    fn spawn_far_animated_door(app: &mut App, source: Entity) -> Door {
        let far = animated_door(app);
        let row = far_row(app, source);
        app.world_mut().entity_mut(far.door).insert(row);
        far
    }

    /// The far door of the link for a model with nothing to animate.
    fn spawn_far_static_door(app: &mut App, source: Entity) -> Entity {
        let row = far_row(app, source);
        app.world_mut()
            .spawn((row, DoorState::Closed, DoorAnimation::default()))
            .id()
    }

    /// The crossing of `source`'s doorway: the [`OpenDestinationDoor`] `crate::transition` writes
    /// the frame the player's feet reach the doorway's plane.
    fn cross(app: &mut App, source: Entity) {
        let destination_ref_id = far_ref(app, source);
        app.world_mut().write_message(OpenDestinationDoor {
            door: source,
            destination_ref_id,
        });
        app.update();
    }

    /// How far through its `Open` clip a door's swing has got: the pose the window shows in the
    /// doorway it is drawn in, which is the animation's own seek time (`animate_targets` evaluates
    /// every curve there) and not its elapsed time - a clip a crossing seeked into would read as
    /// barely started.
    fn swing_fraction(app: &App, door: &Door) -> Option<f32> {
        let animation = player(app, door.player).animation(door.open_node)?;
        Some((animation.seek_time() / CLIP_SECONDS).clamp(0.0, 1.0))
    }

    /// A mapped crossing through an anchored door arrives **in the destination doorway**, which is
    /// where the far door of the link stands. Through the source doorway that door was out of the
    /// way, so it has to be out of the way in the frame the player arrives too - at the pose the
    /// window left it: the point of its own `Open` clip the source door's clip had reached. A door
    /// swung from its rest pose there would swing *through* the player, and a door left closed is the
    /// back of a leaf at point-blank range with the walk out of the doorway blocked behind it.
    #[test]
    fn a_mapped_crossing_opens_the_far_door_where_the_window_left_it() {
        let mut app = door_app();
        let source = animated_door(&mut app);
        let far = spawn_far_animated_door(&mut app, source.door);
        // `E` and four frames of the swing: the doorway is a way through
        // ([`DoorState::is_open`]) with the clip still short of the mark the state calls open, so
        // the window is drawing a half-open door in the destination doorway.
        activate(&mut app, source.door);
        step(&mut app, 4);
        let showed = swing_fraction(&app, &source).expect("the source door is swinging");
        assert!(showed < OPEN_FRACTION, "the swing is under way: {showed}");
        assert_eq!(
            state(&app, far.door),
            DoorState::Closed,
            "and the far door is shut to begin with"
        );

        cross(&mut app, source.door);

        assert_eq!(
            state(&app, far.door),
            DoorState::Opening,
            "the far door arrives where the window left it, mid-swing rather than shut"
        );
        let arrived = swing_fraction(&app, &far).expect("the far door's clip is playing");
        assert!(
            (arrived - showed).abs() <= 2.0 * STEP_SECONDS,
            "the far door picks its swing up at {showed}, the point the window showed, rather than \
             at the rest pose: {arrived}"
        );
        assert!(
            probe_skips(&mut app, far.leaf_mesh),
            "so the player's landing is not inside the far door's leaf: the doorway is one to walk \
             out of"
        );

        // And its swing carries on from there: it is `Open` the frame the clip passes the mark,
        // exactly as a door the player opened themselves is.
        step(&mut app, 3);
        assert_eq!(state(&app, far.door), DoorState::Open { animated: true });
    }

    /// The ordinary arrival: the door the player is walking through has stood open since they asked
    /// for it, so the window showed a doorway with nothing in it and the far door is simply open -
    /// put at the end of its own clip, not swung through the player.
    #[test]
    fn a_crossing_through_an_open_door_arrives_at_an_open_far_door() {
        let mut app = door_app();
        let source = animated_door(&mut app);
        let far = spawn_far_animated_door(&mut app, source.door);
        activate(&mut app, source.door);
        step(&mut app, 12);
        assert_eq!(
            state(&app, source.door),
            DoorState::Open { animated: true },
            "the door the player walks through has stood open"
        );

        cross(&mut app, source.door);

        assert_eq!(
            state(&app, far.door),
            DoorState::Open { animated: true },
            "the far door arrives open"
        );
        assert_eq!(
            swing_fraction(&app, &far),
            Some(1.0),
            "at the end of its own clip, the pose the open source door's mirror was drawn in"
        );
        assert!(
            probe_skips(&mut app, far.leaf_mesh),
            "and its leaf is out of the doorway the player lands in"
        );
    }

    /// A far door with no animation of its own has no leaf that can swing out of the doorway, so the
    /// whole reference goes: the hole a static door opens as, and the one the window showed (the
    /// portal hid that reference while it drew the destination through the source doorway).
    #[test]
    fn a_crossing_arrives_at_a_far_door_with_nothing_to_animate_as_a_hole() {
        let mut app = door_app();
        let source = animated_door(&mut app);
        let far = spawn_far_static_door(&mut app, source.door);
        activate(&mut app, source.door);
        step(&mut app, 12);

        cross(&mut app, source.door);

        assert_eq!(state(&app, far), DoorState::Open { animated: false });
        assert!(
            state(&app, far).hides_whole_reference(),
            "which is the hole `crate::portal` draws an open door with no animation as"
        );
    }

    /// A far door that is already open is left exactly where it is: whatever opened it, the window
    /// showed the player a doorway, and the doorway is what they walked into. The arrival is not a
    /// second activation of a door that never shut.
    #[test]
    fn an_arrival_leaves_a_far_door_that_is_already_open_as_it_is() {
        let mut app = door_app();
        let source = animated_door(&mut app);
        let far = spawn_far_animated_door(&mut app, source.door);
        activate(&mut app, far.door);
        step(&mut app, 12);
        assert_eq!(state(&app, far.door), DoorState::Open { animated: true });
        activate(&mut app, source.door);
        step(&mut app, 2);

        cross(&mut app, source.door);

        assert!(
            player(&app, far.player)
                .animation(far.open_node)
                .expect("the far door's clip")
                .is_finished(),
            "the far door's own swing is left where it was - the arrival does not restart it"
        );
        assert_eq!(state(&app, far.door), DoorState::Open { animated: true });
    }

    /// The destination is streamed in before a crossing is applied, so the far door is normally
    /// there. When it is not, the arrival is held for it and applied the frame it appears; a crossing
    /// whose door never turns up is dropped without a word, because there is nothing to open and
    /// nothing to report.
    #[test]
    fn an_arrival_waits_for_its_far_door_and_gives_up_silently() {
        let mut app = door_app();
        let source = animated_door(&mut app);
        activate(&mut app, source.door);
        step(&mut app, 12);

        // The crossing lands before the destination cell has spawned the door it names.
        cross(&mut app, source.door);
        assert_eq!(
            app.world().resource::<ArrivalOpenings>().openings.len(),
            1,
            "with no door to open, the arrival is held for it"
        );

        step(&mut app, 2);
        let far = spawn_far_animated_door(&mut app, source.door);
        assert_eq!(state(&app, far.door), DoorState::Closed);
        step(&mut app, 1);
        assert_eq!(
            state(&app, far.door),
            DoorState::Open { animated: true },
            "the held arrival opens the door the frame it is there"
        );
        assert!(
            app.world()
                .resource::<ArrivalOpenings>()
                .openings
                .is_empty(),
            "and is not held any more"
        );

        // A door that never turns up (a link into a cell whose reference is not in the world): the
        // arrival is given up on, and the door that is there is not touched.
        let missing = far_ref(&app, source.door) + 1;
        app.world_mut().write_message(OpenDestinationDoor {
            door: source.door,
            destination_ref_id: missing,
        });
        app.update();
        assert_eq!(app.world().resource::<ArrivalOpenings>().openings.len(), 1);
        step(&mut app, ARRIVAL_OPEN_WAIT_FRAMES + 1);
        assert!(
            app.world()
                .resource::<ArrivalOpenings>()
                .openings
                .is_empty(),
            "a far door that never comes is given up on"
        );
        assert_eq!(
            state(&app, far.door),
            DoorState::Open { animated: true },
            "and nothing else was opened in the meantime"
        );
    }

    #[test]
    fn reactivating_an_open_door_closes_it() {
        let mut app = door_app();
        let door = animated_door(&mut app);
        activate(&mut app, door.door);
        step(&mut app, 6);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });

        activate(&mut app, door.door);

        assert_eq!(state(&app, door.door), DoorState::Closing);
        assert!(
            player(&app, door.player).is_playing_animation(door.close_node),
            "the close clip plays"
        );
        assert!(
            player(&app, door.player).is_playing_animation(door.open_node),
            "and the open one is still there, fading out rather than stopping dead"
        );

        // A second `E` while the door is closing is ignored: the rest pose is the pose a `Close`
        // clip starts from, so a door cannot be turned around to open again mid-close.
        activate(&mut app, door.door);
        assert_eq!(state(&app, door.door), DoorState::Closing);

        step(&mut app, 11);
        assert_eq!(
            state(&app, door.door),
            DoorState::Closed,
            "the close clip runs out and the door is shut again"
        );
        assert!(
            !player(&app, door.player).is_playing_animation(door.open_node),
            "the faded-out clip is stopped once it has no weight left"
        );
    }

    /// Reversing a door must not throw it to the other end of the swing: `Open` is reached halfway
    /// through the clip, so a door told to close at 0.6 s is 60% of the way open, and starting
    /// `Close` at its own zero (which *is* fully open) would move the leaf further in one frame than
    /// the whole swing's remaining time allows.
    #[test]
    fn a_reversal_continues_from_the_pose_the_door_reached() {
        let mut app = door_app();
        let door = animated_door(&mut app);
        activate(&mut app, door.door);
        step(&mut app, 6);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });

        let reached = player(&app, door.player)
            .animation(door.open_node)
            .expect("the open clip")
            .elapsed()
            / CLIP_SECONDS;
        assert!(reached > 0.0 && reached < 1.0, "mid-swing: {reached}");
        // The leaf really is part way through the swing - otherwise "the pose did not move much"
        // would be true of a leaf that never moved at all, and the test would prove nothing.
        let before = leaf_degrees(&app, door.leaf);
        assert!(
            before > WIDE_SWING_DEGREES * 0.4 && before < WIDE_SWING_DEGREES,
            "the leaf is part way open, not at either end: {before}"
        );

        activate(&mut app, door.door);

        assert_eq!(state(&app, door.door), DoorState::Closing);
        let close = player(&app, door.player)
            .animation(door.close_node)
            .expect("the close clip");
        assert!(
            (close.seek_time() - (1.0 - reached)).abs() <= STEP_SECONDS + 0.01,
            "the close clip starts at the pose the open one reached: {:?} for {reached}",
            close.seek_time()
        );
        let fading = player(&app, door.player)
            .animation(door.open_node)
            .expect("the open clip is still in the player while it fades")
            .weight();
        assert!(
            fading < 1.0,
            "the clip being left fades out over {} ms: weight {fading}",
            CLIP_FADE.as_millis()
        );

        let after = leaf_degrees(&app, door.leaf);
        let moved = (after - before).abs();
        let per_frame = STEP_SECONDS * WIDE_SWING_DEGREES;
        assert!(
            moved <= 2.0 * per_frame,
            "the leaf moved {moved} degrees in the reversal frame, where one frame of the swing is \
             {per_frame} degrees and starting `Close` at its own zero would have snapped the rest \
             of the swing in that one frame: {before} -> {after}"
        );
    }

    /// The model as `bevy_gltf` builds it for a door whose `Open` clip moves `Object02`: the clips
    /// name that node only, while the loader's `AnimationTargetId` is on every node under the
    /// animation root, the frame included.
    #[test]
    fn door_leaf_marks_the_nodes_the_clips_move_and_not_the_door_frame() {
        let mut app = door_app();
        let model = door_with_model(&mut app, NARROW_SWING_DEGREES);
        let ModelDoor {
            door,
            player,
            moved,
            moved_mesh,
            frame,
        } = model;

        app.update();

        let world = app.world();
        assert!(
            world
                .get::<DoorLeaf>(moved)
                .is_some_and(|leaf| leaf.door == door),
            "the node the clip moves is the leaf"
        );
        assert!(
            world.get::<DoorLeaf>(frame).is_none(),
            "a node with an AnimationTargetId the clips never name is not"
        );
        assert!(
            world.get::<DoorLeaf>(door).is_none() && world.get::<DoorLeaf>(moved_mesh).is_none(),
            "and neither is the door reference nor a mesh primitive - the leaf is the marked node"
        );
        let leaves: Vec<&DoorLeaf> = world
            .iter_entities()
            .filter_map(|e| e.get::<DoorLeaf>())
            .collect();
        assert_eq!(leaves.len(), 1, "exactly the marked node: {leaves:?}");
        assert!(
            world
                .get::<AnimationGraphHandle>(player)
                .is_some_and(|handle| world
                    .resource::<Assets<AnimationGraph>>()
                    .contains(handle.id())),
            "the graph the loader does not attach is on the player entity"
        );
        assert!(
            world.get::<AnimationTransitions>(player).is_some(),
            "and the transitions that fade a reversal are beside it"
        );
        assert!(
            world.get::<PendingDoorModel>(door).is_none(),
            "a resolved door is no longer waiting for its model"
        );
    }

    /// Whether a door's leaves are hidden when it opens is a fact about the `Open` clip it ends up
    /// with - how far that clip turns them - so it is read off the clip at the moment the door is
    /// given its animation, not guessed from the state or watched while the door swings.
    ///
    /// The clip a door ends up with is not always the clip its model carried, of course: a narrow
    /// swing is scaled up to open the doorway ([`swung_open_clips`]), so a model whose clip turns
    /// its leaf 8 degrees also arrives clearing it. A clip that turns its leaf nowhere at all has
    /// nothing to scale and does not clear - which is the case this test keeps for the leaves being
    /// hidden at all.
    #[test]
    fn the_clip_tells_the_door_whether_its_leaves_clear_the_doorway() {
        for (swing, clears) in [
            (0.0, false),
            (NARROW_SWING_DEGREES, true),
            (DOORWAY_CLEAR_DEGREES, true),
            (WIDE_SWING_DEGREES, true),
        ] {
            let mut app = door_app();
            let model = door_with_model(&mut app, swing);

            app.update();

            let animation = *app
                .world()
                .get::<DoorAnimation>(model.door)
                .expect("a resolved door");
            assert_eq!(
                animation.clears_doorway,
                clears,
                "{swing} degrees {} clear the doorway: {animation:?}",
                if clears { "does" } else { "does not" }
            );
        }
    }

    /// `Open`/`Close` is a convention rather than a guarantee - the install also has `open`,
    /// `close`, `Forward`, `Backward`, `Idle` and `Stage1` - so the engine matches the two names
    /// case-insensitively and a model that names neither gets its first clip as its opening one.
    #[test]
    fn door_clips_are_named_case_insensitively_with_the_first_clip_as_a_fallback() {
        let mut app = door_app();
        let mut add = |named: bool| {
            [
                (
                    if named { "open" } else { "Stage1" },
                    app.world_mut()
                        .resource_mut::<Assets<AnimationClip>>()
                        .add(clip(CLIP_SECONDS)),
                ),
                (
                    "Close",
                    app.world_mut()
                        .resource_mut::<Assets<AnimationClip>>()
                        .add(clip(CLIP_SECONDS)),
                ),
            ]
        };

        let lower = add(true);
        let model = gltf(&[lower[0].1.clone(), lower[1].1.clone()], &lower);
        let (open, close) = door_clips(&model);
        assert_eq!(open, Some(lower[0].1.clone()), "`open` is found by name");
        assert_eq!(close, Some(lower[1].1.clone()), "`Close` too");

        let other = add(false);
        let model = gltf(&[other[0].1.clone(), other[1].1.clone()], &other);
        let (open, close) = door_clips(&model);
        assert_eq!(
            open,
            Some(other[0].1.clone()),
            "a model that names no `open` opens with its first clip"
        );
        assert_eq!(close, Some(other[1].1.clone()), "and closes with `Close`");

        let model = gltf(&[], &[]);
        assert_eq!(
            (door_clips(&model).0, door_clips(&model).1),
            (None, None),
            "no clips at all: a static door"
        );
    }

    /// The state the assets are in while the converted door models carry no clips yet: the
    /// resolution has to come out static rather than wait for a clip that will never arrive.
    #[test]
    fn a_model_with_no_clips_resolves_to_a_static_door() {
        let mut app = door_app();
        let model = app
            .world_mut()
            .resource_mut::<Assets<Gltf>>()
            .add(gltf(&[], &[]));
        let door = app
            .world_mut()
            .spawn((load_door(false), DoorState::Closed, pending(model)))
            .id();

        app.update();

        let animation = *app
            .world()
            .get::<DoorAnimation>(door)
            .expect("a resolved door");
        assert!(
            animation.open.is_none() && animation.player.is_none(),
            "nothing to play: {animation:?}"
        );
        assert!(app.world().get::<PendingDoorModel>(door).is_none());

        activate(&mut app, door);
        assert_eq!(state(&app, door), DoorState::Open { animated: false });
    }

    /// A load door whose base has no model at all: static from its first frame, with no asset
    /// server in the path.
    #[test]
    fn a_door_with_no_model_is_static_from_its_first_frame() {
        let mut app = door_app();
        let door = app.world_mut().spawn(load_door(false)).id();

        app.update();

        assert_eq!(state(&app, door), DoorState::Closed);
        assert!(app.world().get::<DoorAnimation>(door).is_some());
        assert!(app.world().get::<PendingDoorModel>(door).is_none());

        activate(&mut app, door);
        assert_eq!(state(&app, door), DoorState::Open { animated: false });
    }

    /// A door whose clips the loader never gives it a player for - a scene that failed to spawn, or
    /// one the loader found no animation root in - must not stay unresolved for the life of the
    /// cell: it is given up on and behaves as a static door, which is what the player sees anyway.
    #[test]
    fn a_door_whose_scene_never_appears_is_given_up_on_as_a_static_door() {
        let mut app = door_app();
        let open = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip(CLIP_SECONDS));
        let model = app.world_mut().resource_mut::<Assets<Gltf>>().add(gltf(
            std::slice::from_ref(&open),
            &[(OPEN_CLIP, open.clone())],
        ));
        let door = app
            .world_mut()
            .spawn((load_door(false), DoorState::Closed, pending(model)))
            .id();

        // A scene that is a frame or two late must not be written off.
        step(&mut app, 5);
        assert!(app.world().get::<DoorAnimation>(door).is_none());
        assert!(app.world().get::<PendingDoorModel>(door).is_some());

        step(&mut app, SCENE_WAIT_FRAMES);
        assert!(
            app.world().get::<PendingDoorModel>(door).is_none(),
            "the wait is given up on"
        );
        assert!(
            app.world()
                .get::<DoorAnimation>(door)
                .is_some_and(|animation| animation.open.is_none()),
            "and the door is static from here on"
        );

        activate(&mut app, door);
        assert_eq!(state(&app, door), DoorState::Open { animated: false });
    }

    /// The published door models of the demo route, read the way [`crate::render`]'s tests read
    /// glTF: whatever clips they turn out to carry, a door model with clips has to resolve to one
    /// the engine can play, and a model without any has to stay static.
    ///
    /// Today the route's models carry none - the `Open`/`Close` clips are not converted yet - and
    /// the assertion is written so that it keeps meaning something on the day they do.
    ///
    /// The converted assets are game data and are never committed, so this test is opt-in: it is
    /// `#[ignore]`d and skips - printing why - when `OPENSKYRIM_CONVERTED_DIR` does not name a
    /// converted asset tree, so CI never needs proprietary data (ADR-0002).
    #[test]
    #[ignore = "reads the converted Skyrim door models (OPENSKYRIM_CONVERTED_DIR)"]
    fn the_route_door_models_resolve_to_a_clip_whenever_they_carry_one() {
        let Some(assets) =
            std::env::var_os("OPENSKYRIM_CONVERTED_DIR").map(std::path::PathBuf::from)
        else {
            eprintln!("skipping: set OPENSKYRIM_CONVERTED_DIR to a converted asset tree");
            return;
        };
        if !assets.is_dir() {
            eprintln!("skipping: {} is not a directory", assets.display());
            return;
        }
        const MODELS: [&str; 2] = [
            "meshes/dungeons/dwemer/door/dwemerlargedoorload01.glb",
            "meshes/dungeons/dwemer/door/dwemersmalldoorload01.glb",
        ];
        for model_path in MODELS {
            let path = assets.join(model_path);
            let Ok(bytes) = std::fs::read(&path) else {
                eprintln!("skipped: {} is not installed", path.display());
                continue;
            };
            let document = bevy::gltf::gltf::Gltf::from_slice(&bytes).expect("a valid .glb");

            // The model as the loader would hand it over: its animations, named as the glTF
            // document names them.
            let mut app = door_app();
            let mut handles = Vec::new();
            let mut named = Vec::new();
            for animation in document.animations() {
                let handle = app
                    .world_mut()
                    .resource_mut::<Assets<AnimationClip>>()
                    .add(clip(CLIP_SECONDS));
                if let Some(name) = animation.name() {
                    named.push((name.to_string(), handle.clone()));
                }
                handles.push(handle);
            }
            let named: Vec<(&str, Handle<AnimationClip>)> = named
                .iter()
                .map(|(name, handle)| (name.as_str(), handle.clone()))
                .collect();
            let model = gltf(&handles, &named);

            let (open, _) = door_clips(&model);
            assert_eq!(
                open.is_some(),
                !handles.is_empty(),
                "{model_path} carries {} clips: a door model with clips must resolve to one the \
                 engine can play, and one without must stay static",
                handles.len()
            );
        }
    }

    /// The route's own door, as the converter actually bakes it, through the real glTF loader: its
    /// `Open` clip turns its two leaves 5.36 and 8.74 degrees, its `Close` is that swing run
    /// backwards, and scaling the `Open` the way the engine scales a load door's clip stands the
    /// door open at [`SWUNG_OPEN_DEGREES`] with every leaf's first pose exactly where it was.
    ///
    /// This is the test the reconversion makes possible - until the door models carried clips there
    /// was nothing to scale - and it is the closest thing to a runtime check of path 2 that does not
    /// need a window: it measures the clip the engine would play, not a fixture that stands in for
    /// it. Opt-in like the test above, and skipped when `OPENSKYRIM_CONVERTED_DIR` is not set
    /// (ADR-0002).
    #[test]
    #[ignore = "reads the converted Skyrim door models (OPENSKYRIM_CONVERTED_DIR)"]
    fn the_route_door_s_own_clip_scales_to_a_doorway() {
        let Some(assets) =
            std::env::var_os("OPENSKYRIM_CONVERTED_DIR").map(std::path::PathBuf::from)
        else {
            eprintln!("skipping: set OPENSKYRIM_CONVERTED_DIR to a converted asset tree");
            return;
        };
        if !assets.is_dir() {
            eprintln!("skipping: {} is not a directory", assets.display());
            return;
        }

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin {
                file_path: assets.to_string_lossy().into_owned(),
                ..default()
            },
            bevy::gltf::GltfPlugin::default(),
            bevy::animation::AnimationPlugin,
            bevy::scene::ScenePlugin,
            bevy::render::mesh::MeshPlugin,
        ));
        // The glTF loader produces these asset kinds; without their types registered the load
        // fails before a clip is ever built.
        app.init_asset::<bevy::image::Image>()
            .init_asset::<bevy::pbr::StandardMaterial>();
        let handle: Handle<Gltf> = app
            .world()
            .resource::<AssetServer>()
            .load("meshes/dungeons/dwemer/door/dwemerlargedoorload01.glb");
        // Loading is asynchronous, and what this test is about is the clip the loader builds.
        for _ in 0..600 {
            app.update();
            if app
                .world()
                .resource::<Assets<Gltf>>()
                .get(&handle)
                .is_some()
            {
                break;
            }
            // The load runs on the IO pool; spinning `update` alone outpaces it.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let Some(model) = app.world().resource::<Assets<Gltf>>().get(&handle) else {
            // A headless test app is not the engine: the glTF loader needs render-side plugins
            // this app cannot add, so a model that loads perfectly at runtime can fail to build
            // here. The engine's own runs are what check this end to end (the demo tour opens
            // every route door). Skip rather than fail on a harness limit.
            eprintln!(
                "skipping: the glTF loader did not build the model in a headless app ({:?})",
                app.world()
                    .resource::<AssetServer>()
                    .get_load_state(&handle)
            );
            return;
        };
        let (open, close) = door_clips(model);
        let (open, close) = (
            open.expect("`DweDoorLarge01Load` carries an `Open` clip"),
            close.expect("and a `Close` clip"),
        );
        let clips = app.world().resource::<Assets<AnimationClip>>();
        let open = clips.get(&open).expect("the `Open` clip's asset");
        let close = clips.get(&close).expect("the `Close` clip's asset");

        // What the design note measured in the NIF: 5.36 and 8.74 degrees, the widest leaf setting
        // the scale (design section 1.4).
        let moved = clip_targets([open, close]);
        let swing = swing_degrees(open, &moved);
        assert!(
            (swing - 8.74).abs() < 0.05,
            "the door's own swing is {swing} degrees, and the design note says 8.74"
        );
        assert!(
            close_mirrors_open(open, close, &moved),
            "`DweDoorLarge01Load`'s `Close` is its `Open` run backwards"
        );

        let factor = swing_scale(swing);
        assert!(
            (factor - 10.30).abs() < 0.05,
            "so the door is scaled by {factor}, which is {} times its own swing",
            SWUNG_OPEN_DEGREES / swing
        );
        let (swung, closed) = swung_open_clips(Some(open), Some(close), &moved, factor)
            .expect("the door's own clips are a rotation each, and scale");
        let opened = swing_degrees(&swung, &moved);
        assert!(
            (opened - SWUNG_OPEN_DEGREES).abs() < 1.0,
            "the scaled swing stands the door at {opened} degrees"
        );
        assert_eq!(
            swung.curves().keys().copied().collect::<HashSet<_>>(),
            moved,
            "and turns the same leaves the door's own clip turned"
        );

        // Every leaf starts exactly where the door's own clip starts it: the swing is longer, not
        // moved.
        let rotation = animated_field!(Transform::rotation);
        let closed = closed.expect("the scaled close");
        for target in &moved {
            for clip in [&swung, &closed] {
                let (Some(rest), Some(scaled_rest)) = (
                    open.sample_clamped(rotation.clone(), *target, 0.0),
                    clip.sample_clamped(rotation.clone(), *target, 0.0),
                ) else {
                    panic!("both clips turn every leaf the door's own clip turned");
                };
                let drift = rest.angle_between(scaled_rest).to_degrees();
                assert!(
                    drift < 0.01,
                    "first pose of a leaf moved by {drift} degrees"
                );
            }
        }

        // And one leaf of the door's model, named as the loader names it: the path hash the twin's
        // clips have to be rebuilt onto (`twin_target_ids`) is this one, computed from the glTF's
        // own node names - `Creation-to-glTF basis` over `DwemerLargeDoorLoad01` over `Object02`.
        let leaf_path: Vec<Name> = [
            "Creation-to-glTF basis",
            "DwemerLargeDoorLoad01",
            "Object02",
        ]
        .iter()
        .map(|name| Name::new(*name))
        .collect();
        assert!(
            moved.contains(&AnimationTargetId::from_names(leaf_path.iter())),
            "the loader's target id is a hash of the node's whole name path"
        );
    }

    #[test]
    fn an_auto_load_door_is_born_open_and_never_changes() {
        let mut app = door_app();
        let door = app
            .world_mut()
            .spawn((
                load_door(true),
                MeshHandle("meshes/dungeons/dwemer/door/dwemerlargedoorload01.glb".into()),
            ))
            .id();

        app.update();
        assert_eq!(state(&app, door), DoorState::Open { animated: false });
        assert!(
            app.world().get::<PendingDoorModel>(door).is_none(),
            "an invisible marker loads no model"
        );

        activate(&mut app, door);
        step(&mut app, 5);
        assert_eq!(
            state(&app, door),
            DoorState::Open { animated: false },
            "a marker crosses the player on contact and has no animation to run"
        );
    }

    /// The walk probe's question: is this mesh a leaf its door has taken out of the doorway?
    #[test]
    fn a_mesh_under_a_leaf_is_out_of_the_way_only_while_its_door_is_open() {
        let mut app = door_app();
        let door = animated_door(&mut app);

        // The fixture is what `attach_door_animations` leaves behind: the moving node marked, the
        // frame beside it not.
        assert!(app.world().get::<DoorLeaf>(door.leaf).is_some());
        assert!(app.world().get::<DoorLeaf>(door.frame).is_none());
        assert!(
            !probe_skips(&mut app, door.leaf_mesh),
            "a closed door's leaf is something to walk into"
        );
        assert!(
            !probe_skips(&mut app, door.frame_mesh),
            "the frame is not a leaf at all"
        );

        activate(&mut app, door.door);

        assert!(
            probe_skips(&mut app, door.leaf_mesh),
            "an open door's leaf is not something to walk into, drawn or not: {door:?}",
            door = state(&app, door.door)
        );
        assert!(
            !probe_skips(&mut app, door.frame_mesh),
            "and the frame still is not, so the doorway is still a doorway"
        );
    }

    /// [`mesh_is_out_of_the_way`] as `player_walk` calls it, with the three queries it needs.
    fn probe_skips(app: &mut App, mesh: Entity) -> bool {
        let mut state = SystemState::<(Query<&ChildOf>, Query<&DoorLeaf>, Query<&DoorState>)>::new(
            app.world_mut(),
        );
        let (parents, leaves, states) = state.get(app.world()).expect("read-only queries");
        crate::doors::mesh_is_out_of_the_way(mesh, &parents, &leaves, &states)
    }

    /// The doors the player's doorway trigger asked to cross, in order.
    #[derive(Resource, Default)]
    struct Crossed(Vec<Entity>);

    fn collect_crossings(mut crossed: ResMut<Crossed>, mut requests: MessageReader<CrossDoor>) {
        for message in requests.read() {
            crossed.0.push(message.door);
        }
    }

    /// One frame of the player's walk: their eye is put where a player whose feet are at `feet`
    /// would have it - the trigger reads the camera, not the keys - and the frame runs.
    fn walk(app: &mut App, camera: Entity, feet: Vec3) {
        let eye = eye_from_feet(feet);
        app.world_mut().entity_mut(camera).insert((
            Transform::from_translation(eye),
            GlobalTransform::from_translation(eye),
        ));
        app.update();
    }

    /// The feature end to end, in one test: `E` on a door whose model carries an `Open` clip starts
    /// that clip, the door becomes a way through halfway through the swing, and the player's
    /// doorway trigger crosses them the frame their feet reach the plane. Neither half says anything
    /// on its own - the state machine that opens a door nobody walks through, or a crossing that
    /// fires on a door that never opened - so this is the test that the two are one feature.
    ///
    /// The `E` press is the message the controller writes for it ([`crate::player::player_door`]),
    /// and the trigger is the real one, added over the door's own app the way `PlayerPlugin` adds it.
    #[test]
    fn e_opens_the_door_and_the_walk_through_it_crosses_once_it_is_open() {
        // The doorway the fixture's door measures with no bounds of its own: `AUTO_DOOR_MARKER_SIZE`
        // around the reference, which is 160 x 240 wide and tall - so a player at 300 units is
        // outside it and 30 units past the reference is through the plane.
        let base = Vec3::new(1000.0, 0.0, 1000.0);
        // The fixture's door faces east (`outward` is Creation +x), so the player walks in from the
        // east and the doorway's plane is the reference's own.
        let in_front = base + Vec3::X * 300.0;
        let through = base - Vec3::X * 30.0;

        let mut app = door_app();
        let door = animated_door(&mut app);
        app.world_mut().entity_mut(door.door).insert((
            Transform::from_translation(base),
            GlobalTransform::from_translation(base),
        ));
        let camera = app
            .world_mut()
            .spawn((
                StreamingCamera,
                Player::default(),
                Transform::from_translation(eye_from_feet(in_front)),
                GlobalTransform::from_translation(eye_from_feet(in_front)),
            ))
            .id();
        app.init_resource::<ProfilingState>()
            .init_resource::<Crossed>()
            .add_message::<CrossDoor>()
            .add_message::<DoorCrossed>()
            .add_systems(
                Update,
                (player_walks_through_doors, collect_crossings).chain(),
            );

        // A closed door has no way through it: the walk over its plane fires nothing at all.
        assert_eq!(state(&app, door.door), DoorState::Closed);
        app.update();
        walk(&mut app, camera, through);
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "a closed door is not walked through, however it is crossed"
        );

        // `E` on it, which is `OpenDoor`: the door's own clip starts, and the doorway is not a way
        // through because `E` was pressed - it is one because the door is open.
        walk(&mut app, camera, in_front);
        app.world_mut().write_message(OpenDoor { door: door.door });
        app.update();
        assert_eq!(
            state(&app, door.door),
            DoorState::Opening,
            "E starts the swing; it does not open the door in one frame"
        );
        assert!(
            player(&app, door.player).is_playing_animation(door.open_node),
            "and the clip playing is the door model's own `Open`"
        );

        // Half a clip later it is open, holding its last key, and the player has still not moved.
        step(&mut app, 6);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        assert!(
            app.world().resource::<Crossed>().0.is_empty(),
            "nothing crosses while the player stands in front of the doorway"
        );

        // The same walk over the same plane, now that the door is open, crosses them - on the
        // doorway's plane, which is where the window the portal draws through ends.
        walk(&mut app, camera, through);
        assert_eq!(
            app.world().resource::<Crossed>().0,
            vec![door.door],
            "the open door is the one the crossing fires on"
        );
    }

    /// A door holding a crossing draws its leaves however far the clip swung them: the doorway is a
    /// shut door until the destination it is waiting for streams in, and there is no window through
    /// it either.
    #[test]
    fn a_door_holding_its_crossing_keeps_its_leaves_drawn() {
        let narrow = DoorAnimation {
            clears_doorway: false,
            ..default()
        };
        assert!(
            !leaves_are_drawn(DoorState::Open { animated: true }, Some(&narrow), false),
            "an 8-degree clip leaves its leaves in the doorway, and they go once it is open"
        );
        assert!(
            leaves_are_drawn(DoorState::Open { animated: true }, Some(&narrow), true),
            "unless the door is waiting for its destination, where the doorway is a shut door"
        );
    }

    /// A door with an open clip and no close clip replays the open one instead of closing
    /// (design section 5).
    #[test]
    fn a_door_without_a_close_clip_replays_its_open_instead_of_closing() {
        let mut app = door_app();
        let door = animated_door(&mut app);
        app.world_mut()
            .entity_mut(door.door)
            .get_mut::<DoorAnimation>()
            .expect("the fixture's animation")
            .close = None;
        activate(&mut app, door.door);
        step(&mut app, 6);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        let held = player(&app, door.player)
            .animation(door.open_node)
            .expect("the open clip")
            .elapsed();

        activate(&mut app, door.door);

        assert_eq!(state(&app, door.door), DoorState::Opening);
        let replay = player(&app, door.player)
            .animation(door.open_node)
            .expect("the open clip is playing again");
        assert!(
            !replay.is_finished(),
            "the clip runs again rather than staying finished where it stopped"
        );
        assert!(
            replay.elapsed() < held,
            "and from the beginning: {} after {}",
            replay.elapsed(),
            held
        );
    }

    /// The swing is what the player asked to see, so a door's leaves are drawn while the `Open`
    /// clip plays - and a clip that turns them a few degrees leaves them in the opening, so they are
    /// hidden when the door is `Open`. Only the leaves: the frame is not a `DoorLeaf`.
    #[test]
    fn a_narrow_clip_leaves_its_leaves_drawn_through_the_swing_and_hidden_once_open() {
        let mut app = door_app();
        let door = animated_door_swinging(&mut app, NARROW_SWING_DEGREES);
        assert!(
            !app.world()
                .get::<DoorAnimation>(door.door)
                .expect("the fixture's animation")
                .clears_doorway,
            "an 8 degree swing does not clear a doorway"
        );
        assert_ne!(
            leaf_visibility(&app, door.leaf),
            Visibility::Hidden,
            "a closed door's leaf is drawn"
        );

        activate(&mut app, door.door);
        step(&mut app, 4);
        assert_eq!(state(&app, door.door), DoorState::Opening);
        assert_ne!(
            leaf_visibility(&app, door.leaf),
            Visibility::Hidden,
            "and it is still drawn while it swings"
        );

        step(&mut app, 2);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        assert_eq!(
            leaf_visibility(&app, door.leaf),
            Visibility::Hidden,
            "the clip did not open the doorway, so the leaf goes"
        );

        // Closing draws it again from the first frame: a leaf that is coming back is a leaf.
        activate(&mut app, door.door);
        assert_eq!(state(&app, door.door), DoorState::Closing);
        assert_ne!(
            leaf_visibility(&app, door.leaf),
            Visibility::Hidden,
            "the closing swing is drawn"
        );

        step(&mut app, 11);
        assert_eq!(state(&app, door.door), DoorState::Closed);
        assert_ne!(leaf_visibility(&app, door.leaf), Visibility::Hidden);
    }

    /// A door that swings its leaf clear of the opening keeps it: the doorway is open, and the leaf
    /// next to it is what a real door looks like when it is open.
    #[test]
    fn a_clip_that_swings_its_leaf_clear_of_the_doorway_leaves_it_drawn() {
        let mut app = door_app();
        let door = animated_door_swinging(&mut app, WIDE_SWING_DEGREES);

        activate(&mut app, door.door);
        step(&mut app, 6);

        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        assert_ne!(
            leaf_visibility(&app, door.leaf),
            Visibility::Hidden,
            "the leaf swung out of the doorway and stays drawn: {} degrees",
            leaf_degrees(&app, door.leaf)
        );
    }

    /// A static door has no leaf to move, so an open one is a hole where its model was - and that
    /// is the portal's line, driven by [`DoorState::hides_whole_reference`]. An animated door never
    /// answers true, whatever its leaves are doing.
    #[test]
    fn only_a_static_door_hides_its_whole_reference() {
        assert!(!DoorState::Closed.hides_whole_reference());
        assert!(!DoorState::Opening.hides_whole_reference());
        assert!(!DoorState::Closing.hides_whole_reference());
        assert!(
            !DoorState::Open { animated: true }.hides_whole_reference(),
            "an animating door keeps its frame: its leaves are the animation's business"
        );
        assert!(
            DoorState::Open { animated: false }.hides_whole_reference(),
            "a door with no clip of its own has nothing to swing out of the way"
        );

        // And `is_open` is what the crossing gates on: usable from the first frame of the swing,
        // for either kind of door, and not while it is closed or closing.
        assert!(!DoorState::Closed.is_open());
        assert!(DoorState::Opening.is_open());
        assert!(!DoorState::Closing.is_open());
        assert!(DoorState::Open { animated: true }.is_open());
        assert!(DoorState::Open { animated: false }.is_open());
    }

    /// The converter's wrapper node, which every converted model's scene hangs under, and the names
    /// of the two models of one load/twin pair - the `CasExFreeSmDoorLoad01` pair, one of the two
    /// pairs in the install whose models really do name their leaves the same thing.
    const BASIS: &str = "Creation-to-glTF basis";
    const MODEL_ROOT: &str = "CasExFreeSmDoorLoad01";
    const TWIN_ROOT: &str = "CasExFreeSmDoor01";
    /// The node the door's own `Open` clip turns.
    const OWN_LEAF: &str = "Door01";

    /// How long the twin's clips last in these tests: twice the door's own, so which of the two
    /// clips a door ended up with is in the state machine's clock and not only in the angle.
    const TWIN_CLIP_SECONDS: f32 = 2.0;

    /// The id the loader would give the node at `path` under the animation root: a hash of the whole
    /// path, the scene root's own name first.
    fn target_id(path: &[&str]) -> AnimationTargetId {
        let names: Vec<Name> = path
            .iter()
            .map(|name| Name::new(name.to_string()))
            .collect();
        AnimationTargetId::from_names(names.iter())
    }

    /// The name path of a node of a converted model, as the loader builds it.
    fn node_path(name: &str) -> Vec<Name> {
        [BASIS, MODEL_ROOT, name]
            .iter()
            .map(|name| Name::new(name.to_string()))
            .collect()
    }

    /// A door of a load/twin pair, and the nodes the two models' clips turn.
    struct TwinDoor {
        door: Entity,
        /// The model path the door carries, which is what the borrow log dedupes on.
        path: String,
        /// The node the door's own `Open` clip turns, a node of the door's own model by
        /// construction.
        own_leaf: Entity,
        /// The node the twin's `Open` clip turns, where the door's model has one by that name.
        twin_leaf: Option<Entity>,
    }

    /// A load door of `MODEL_ROOT` with a non-load twin loaded beside it, laid out the way the
    /// loader lays out a converted model: the animation root is the converter's basis wrapper, the
    /// model's own root node hangs under it, and the leaves under that.
    ///
    /// The two models' clips are aimed at different nodes on purpose. The door's own `Open` clip
    /// turns `OWN_LEAF` by `own_swing` degrees over [`CLIP_SECONDS`]; the twin's turns `twin_leaf` by
    /// `twin_swing` over [`TWIN_CLIP_SECONDS`]. `nodes` are the node names the door's own model has
    /// below its root, so `twin_leaf` is a node of the door's model only when the caller puts it
    /// there - and that is the whole of whether the twin's clip fits this door.
    fn door_with_twin(
        app: &mut App,
        own_swing: f32,
        twin_swing: f32,
        nodes: &[&str],
        twin_leaf: &str,
    ) -> TwinDoor {
        let own = target_id(&[BASIS, MODEL_ROOT, OWN_LEAF]);
        let open = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip_swinging(CLIP_SECONDS, own, 0.0, own_swing));
        let close = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip_swinging(CLIP_SECONDS, own, own_swing, 0.0));
        let model = app.world_mut().resource_mut::<Assets<Gltf>>().add(gltf(
            &[open.clone(), close.clone()],
            &[(OPEN_CLIP, open), (CLOSE_CLIP, close)],
        ));

        let twin = target_id(&[BASIS, TWIN_ROOT, twin_leaf]);
        let twin_open = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip_swinging(TWIN_CLIP_SECONDS, twin, 0.0, twin_swing));
        let twin_close = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip_swinging(TWIN_CLIP_SECONDS, twin, twin_swing, 0.0));
        let twin_model = app.world_mut().resource_mut::<Assets<Gltf>>().add(gltf(
            &[twin_open.clone(), twin_close.clone()],
            &[(OPEN_CLIP, twin_open), (CLOSE_CLIP, twin_close)],
        ));

        let path = format!("meshes/dungeons/castle/lghalls/{MODEL_ROOT}.glb");
        let door = app
            .world_mut()
            .spawn((
                load_door(false),
                DoorState::Closed,
                PendingDoorModel {
                    model,
                    path: path.clone(),
                    twin: Some(TwinModel {
                        path: format!("meshes/dungeons/castle/lghalls/{TWIN_ROOT}.glb"),
                        model: twin_model,
                    }),
                    waiting: 0,
                },
            ))
            .id();
        let player = app
            .world_mut()
            .spawn((
                Name::new(BASIS),
                target_id(&[BASIS]),
                AnimationPlayer::default(),
                ChildOf(door),
            ))
            .id();
        let root = app
            .world_mut()
            .spawn((
                Name::new(MODEL_ROOT),
                target_id(&[BASIS, MODEL_ROOT]),
                AnimatedBy(player),
                Transform::default(),
                ChildOf(player),
            ))
            .id();
        let leaves: Vec<(String, Entity)> = nodes
            .iter()
            .map(|node| {
                let entity = app
                    .world_mut()
                    .spawn((
                        Name::new(node.to_string()),
                        target_id(&[BASIS, MODEL_ROOT, node]),
                        AnimatedBy(player),
                        Transform::default(),
                        Visibility::default(),
                        ChildOf(root),
                    ))
                    .id();
                ((*node).to_owned(), entity)
            })
            .collect();
        let leaf = |wanted: &str| {
            leaves
                .iter()
                .find(|(node, _)| node == wanted)
                .map(|(_, entity)| *entity)
        };
        TwinDoor {
            door,
            path,
            own_leaf: leaf(OWN_LEAF).expect("the door's own model has its own leaf"),
            twin_leaf: leaf(twin_leaf),
        }
    }

    /// The door's own clip of this fixture is the demo route's problem in miniature: it turns the
    /// leaf 8 degrees, which is not a doorway. The twin's clip is the same door's real swing, and it
    /// is rebuilt onto the door's own node and played there.
    #[test]
    fn a_load_door_whose_own_clip_is_narrow_borrows_its_twins_swing() {
        let mut app = door_app();
        let door = door_with_twin(
            &mut app,
            NARROW_SWING_DEGREES,
            WIDE_SWING_DEGREES,
            &[OWN_LEAF, "Plane02"],
            OWN_LEAF,
        );

        app.update();

        let animation = *app
            .world()
            .get::<DoorAnimation>(door.door)
            .expect("a resolved door");
        assert!(
            animation.clears_doorway,
            "the twin's 120 degrees open the doorway where the door's own 8 did not: {animation:?}"
        );
        assert_eq!(
            animation.open.expect("an open clip").seconds,
            TWIN_CLIP_SECONDS,
            "the clip the door ended up with is the twin's, which is twice as long as its own"
        );
        assert!(
            app.world().get::<DoorLeaf>(door.own_leaf).is_some(),
            "the node the borrowed clip turns is marked as the leaf"
        );
        assert!(
            app.world().get::<PendingDoorModel>(door.door).is_none(),
            "and the door is resolved"
        );
        assert_eq!(
            app.world().resource::<AdjustedSwings>().0.get(&door.path),
            Some(&SwingSource::Twin),
            "the twin's clips were the first thing tried, and they were good enough: the door's own \
             clip was never scaled"
        );

        // It is the borrowed clip that plays, on the door's own node: the leaf ends where the twin's
        // clip leaves it, not where the door's own 8-degree clip did.
        activate(&mut app, door.door);
        step(&mut app, 24);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        let degrees = leaf_degrees(&app, door.own_leaf);
        assert!(
            (degrees - WIDE_SWING_DEGREES).abs() < 1.0,
            "the leaf is where the twin's clip took it ({degrees} degrees), not where the door's \
             own clip left it ({NARROW_SWING_DEGREES})"
        );
        assert_ne!(
            leaf_visibility(&app, door.own_leaf),
            Visibility::Hidden,
            "and a leaf that swung clear stays drawn"
        );
    }

    /// A door that can open itself never looks for a twin: its own clip is the swing the player
    /// sees, whichever of the two would open the doorway.
    #[test]
    fn a_door_whose_own_clip_opens_the_doorway_keeps_its_own_clip() {
        let mut app = door_app();
        let door = door_with_twin(
            &mut app,
            WIDE_SWING_DEGREES,
            NARROW_SWING_DEGREES,
            &[OWN_LEAF],
            OWN_LEAF,
        );

        app.update();

        let animation = *app
            .world()
            .get::<DoorAnimation>(door.door)
            .expect("a resolved door");
        assert!(animation.clears_doorway);
        assert_eq!(
            animation.open.expect("an open clip").seconds,
            CLIP_SECONDS,
            "the door's own clip, not the twin's: {animation:?}"
        );
        assert!(
            app.world().resource::<AdjustedSwings>().0.is_empty(),
            "nothing was done to this door's swing: its own clip already opens the doorway"
        );

        activate(&mut app, door.door);
        step(&mut app, 12);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        let degrees = leaf_degrees(&app, door.own_leaf);
        assert!(
            (degrees - WIDE_SWING_DEGREES).abs() < 1.0,
            "the door turned its leaf its own way: {degrees} degrees"
        );
    }

    /// The twin's clips have to move nodes the door's own model has, because a clip that names a
    /// node the door has not got swings nothing there.
    ///
    /// This is the demo route's own pair, at the size of the real data: its load door turns
    /// `Object02` and `Object43` and its twin turns `Object45` and `Object47`, so the twin is no use
    /// to it (`tools/research/load_door_twin_coverage.py pairs` finds 17 of the install's 19
    /// twinned models like this, the demo route's among them). The door then gets its swing the
    /// second way instead: its own clip, scaled up.
    #[test]
    fn a_twin_whose_clips_move_other_nodes_is_not_borrowed() {
        let mut app = door_app();
        let door = door_with_twin(
            &mut app,
            NARROW_SWING_DEGREES,
            WIDE_SWING_DEGREES,
            &[OWN_LEAF],
            "Object45",
        );
        assert!(
            door.twin_leaf.is_none(),
            "the door's model has no `Object45`: the twin's own leaf is not in this scene"
        );

        app.update();

        let animation = *app
            .world()
            .get::<DoorAnimation>(door.door)
            .expect("a resolved door");
        assert_eq!(
            animation.open.expect("an open clip").seconds,
            CLIP_SECONDS,
            "the door's own clip, not the twin's (which is twice as long)"
        );
        assert_eq!(
            app.world().resource::<AdjustedSwings>().0.get(&door.path),
            Some(&SwingSource::Scaled),
            "the door's own clip was scaled, not the twin's borrowed"
        );

        // And it is the door's own swing, turned further: its own clip takes the leaf to 8 degrees
        // and the scaled one to 90, and the twin's untouched would have taken it to 120 over two
        // seconds.
        activate(&mut app, door.door);
        step(&mut app, 12);
        assert_eq!(state(&app, door.door), DoorState::Open { animated: true });
        let degrees = leaf_degrees(&app, door.own_leaf);
        assert!(
            (degrees - SWUNG_OPEN_DEGREES).abs() < 1.0,
            "the door's own swing, scaled to open the doorway: {degrees} degrees"
        );
        assert_ne!(
            leaf_visibility(&app, door.own_leaf),
            Visibility::Hidden,
            "a swing that clears the doorway leaves its leaf drawn"
        );
    }

    /// A twin that never loads is not waited for forever: the door gives up on it after
    /// [`TWIN_WAIT_FRAMES`] - much sooner than it would give up on its own scene, because until it
    /// gives up it is a door nobody can walk through - and gets its swing from its own clip instead.
    #[test]
    fn a_twin_that_never_arrives_is_given_up_on_and_the_door_keeps_its_own_clip() {
        let mut app = door_app();
        let door = door_with_twin(
            &mut app,
            NARROW_SWING_DEGREES,
            WIDE_SWING_DEGREES,
            &[OWN_LEAF],
            OWN_LEAF,
        );
        // A handle the asset server never fills in: the model it names is not there.
        let missing = app.world().resource::<Assets<Gltf>>().reserve_handle();
        app.world_mut()
            .entity_mut(door.door)
            .get_mut::<PendingDoorModel>()
            .expect("the fixture's pending door")
            .twin = Some(TwinModel {
            path: "meshes/fixtures/door/door01twin.glb".to_owned(),
            model: missing,
        });

        step(&mut app, 5);
        assert!(
            app.world().get::<PendingDoorModel>(door.door).is_some(),
            "a twin that is a frame or two late must not be written off"
        );

        step(&mut app, TWIN_WAIT_FRAMES);

        let animation = *app
            .world()
            .get::<DoorAnimation>(door.door)
            .expect("a resolved door");
        assert_eq!(
            animation.open.expect("an open clip").seconds,
            CLIP_SECONDS,
            "the door's own clip, not the twin's: {animation:?}"
        );
        assert_eq!(
            app.world().resource::<AdjustedSwings>().0.get(&door.path),
            Some(&SwingSource::Scaled),
            "and it was scaled, since the twin never came"
        );
        assert!(app.world().get::<PendingDoorModel>(door.door).is_none());
    }

    /// A twin's clips are sub-assets of its model and can arrive a frame after it, the way the
    /// door's own clips did: a door waits for them rather than writing the twin off as one that does
    /// not fit and settling for a clip that does not open its doorway.
    #[test]
    fn a_twin_whose_clips_are_a_frame_late_is_waited_for() {
        let mut app = door_app();
        let door = door_with_twin(
            &mut app,
            NARROW_SWING_DEGREES,
            WIDE_SWING_DEGREES,
            &[OWN_LEAF],
            OWN_LEAF,
        );
        // The twin's model, naming clips that have not been loaded yet.
        let open = app
            .world()
            .resource::<Assets<AnimationClip>>()
            .reserve_handle();
        let close = app
            .world()
            .resource::<Assets<AnimationClip>>()
            .reserve_handle();
        let twin = target_id(&[BASIS, TWIN_ROOT, OWN_LEAF]);
        let twin_model = app.world_mut().resource_mut::<Assets<Gltf>>().add(gltf(
            &[open.clone(), close.clone()],
            &[(OPEN_CLIP, open.clone()), (CLOSE_CLIP, close.clone())],
        ));
        app.world_mut()
            .entity_mut(door.door)
            .get_mut::<PendingDoorModel>()
            .expect("the fixture's pending door")
            .twin = Some(TwinModel {
            path: "meshes/fixtures/door/door01twin.glb".to_owned(),
            model: twin_model,
        });

        app.update();
        assert!(
            app.world().get::<DoorAnimation>(door.door).is_none(),
            "the twin's clips are not here yet, so neither is the door's animation"
        );

        // They arrive.
        app.world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .insert(
                open.id(),
                clip_swinging(TWIN_CLIP_SECONDS, twin, 0.0, WIDE_SWING_DEGREES),
            )
            .expect("the clip arrives under the id the twin's model already names");
        app.world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .insert(
                close.id(),
                clip_swinging(TWIN_CLIP_SECONDS, twin, WIDE_SWING_DEGREES, 0.0),
            )
            .expect("and so does the closing clip");
        step(&mut app, 2);

        let animation = *app
            .world()
            .get::<DoorAnimation>(door.door)
            .expect("a resolved door");
        assert!(
            animation.clears_doorway,
            "the twin's swing was borrowed once its clips arrived: {animation:?}"
        );
        assert_eq!(
            animation.open.expect("an open clip").seconds,
            TWIN_CLIP_SECONDS,
            "and it is the twin's clip, not the door's own"
        );
    }

    /// A model path with no `load` marker in it is a door with no twin at all - the door does not
    /// look for one, and does not wait for one either: its own clip is the only one there is, and it
    /// resolves in the frame it is asked. 72 of the 103 models load doors use are like this
    /// (`tools/research/load_door_twin_coverage.py models`), and their swing is path 2's.
    #[test]
    fn a_door_whose_model_name_carries_no_marker_has_no_twin_to_look_for() {
        let mut app = door_app();
        let door = door_with_twin(
            &mut app,
            NARROW_SWING_DEGREES,
            WIDE_SWING_DEGREES,
            &[OWN_LEAF],
            OWN_LEAF,
        );
        // The twin the fixture loaded is taken away with the marker: the question is whether the
        // door goes looking for one at all, not whether one it was handed is good enough.
        {
            let mut entity = app.world_mut().entity_mut(door.door);
            let mut pending = entity
                .get_mut::<PendingDoorModel>()
                .expect("the fixture's pending door");
            pending.path = "meshes/fixtures/door/door01.glb".to_owned();
            pending.twin = None;
        }

        app.update();

        let animation = *app
            .world()
            .get::<DoorAnimation>(door.door)
            .expect("a resolved door in the frame it was asked");
        assert_eq!(
            animation.open.expect("an open clip").seconds,
            CLIP_SECONDS,
            "the door's own clip, resolved at once: {animation:?}"
        );
        assert_eq!(
            app.world()
                .resource::<AdjustedSwings>()
                .0
                .get("meshes/fixtures/door/door01.glb"),
            Some(&SwingSource::Scaled),
            "a door with no twin still gets a swing, from its own clip"
        );
    }

    /// A door model with no clips of its own has nothing to borrow onto, however good its twin's
    /// swing is: `bevy_gltf` gives a scene with no animations no [`AnimationPlayer`] and no
    /// [`AnimationTargetId`]s (`bevy_gltf-0.19.0/src/loader/mod.rs#L1090`, `#L1545`), so there is no
    /// animation root to hang a borrowed clip on. Such a door stays the static door it is today.
    #[test]
    fn a_model_with_no_clips_of_its_own_has_nothing_to_borrow_onto() {
        let mut app = door_app();
        let model = app
            .world_mut()
            .resource_mut::<Assets<Gltf>>()
            .add(gltf(&[], &[]));
        let door = app
            .world_mut()
            .spawn((load_door(false), DoorState::Closed, pending(model)))
            .id();
        app.world_mut()
            .entity_mut(door)
            .get_mut::<PendingDoorModel>()
            .expect("the fixture's pending door")
            .path = format!("meshes/dungeons/castle/lghalls/{MODEL_ROOT}.glb");

        app.update();

        let animation = *app
            .world()
            .get::<DoorAnimation>(door)
            .expect("a resolved door");
        assert!(
            animation.open.is_none() && animation.player.is_none(),
            "a static door, not one waiting on a twin: {animation:?}"
        );
        assert!(
            app.world().resource::<AdjustedSwings>().0.is_empty(),
            "and there was no clip to borrow or to scale"
        );

        activate(&mut app, door);
        assert_eq!(state(&app, door), DoorState::Open { animated: false });
    }

    /// The log says what was done to a model's swing once, not once per door of that model.
    #[test]
    fn an_adjusted_swing_is_logged_once_per_model_and_not_once_per_door() {
        let mut app = door_app();
        let first = door_with_twin(
            &mut app,
            NARROW_SWING_DEGREES,
            WIDE_SWING_DEGREES,
            &[OWN_LEAF],
            OWN_LEAF,
        );
        step(&mut app, 2);
        let second = door_with_twin(
            &mut app,
            NARROW_SWING_DEGREES,
            WIDE_SWING_DEGREES,
            &[OWN_LEAF],
            OWN_LEAF,
        );
        step(&mut app, 2);

        for door in [first.door, second.door] {
            let animation = *app
                .world()
                .get::<DoorAnimation>(door)
                .expect("a resolved door");
            assert!(
                animation.clears_doorway,
                "both doors of the model borrowed the swing: {animation:?}"
            );
        }
        let adjusted = app.world().resource::<AdjustedSwings>();
        assert_eq!(
            adjusted.0.iter().collect::<Vec<_>>(),
            vec![(&first.path, &SwingSource::Twin)],
            "one entry per model, keyed by the door's own model path, saying which path it took"
        );
    }

    /// The path rule, over the real model paths of the install's load doors
    /// (`tools/research/load_door_twin_coverage.py models`, which dumped them from
    /// `skyrim_world.db`): the marker is taken out wherever it sits in the file name, it is
    /// case-insensitive, and a name without one has no twin.
    #[test]
    fn the_twin_path_is_the_model_path_with_the_load_marker_taken_out() {
        for (model, twin) in [
            // The demo route's own door: its twin's leaves are named Object45/Object47, so the
            // twin's clips do not fit this model and it keeps its own clip.
            (
                "meshes/Dungeons/Dwemer/Door/DwemerLargeDoorLoad01.glb",
                Some("meshes/Dungeons/Dwemer/Door/DwemerLargeDoor01.glb"),
            ),
            (
                "meshes/Dungeons/Dwemer/Door/DwemerSmallDoorLoad01.glb",
                Some("meshes/Dungeons/Dwemer/Door/DwemerSmallDoor01.glb"),
            ),
            // `...Load02`: the second door of a pair.
            (
                "meshes/Dungeons/Nordic/Doors/Animated/SmDoor02/NorDoorSmLoad02.glb",
                Some("meshes/Dungeons/Nordic/Doors/Animated/SmDoor02/NorDoorSm02.glb"),
            ),
            // The marker is not always followed by a number.
            (
                "meshes/Dungeons/Imperial/Door/ImpWoodDoorHoleDoorLoad.glb",
                Some("meshes/Dungeons/Imperial/Door/ImpWoodDoorHoleDoor.glb"),
            ),
            // And it can have a direction in it: `LoadUp01`, `LoadDown01`, `LoadExt`.
            (
                "meshes/Dungeons/Dwemer/Facades/DweFacadeLiftLeverLoadUp01.glb",
                Some("meshes/Dungeons/Dwemer/Facades/DweFacadeLiftLeverUp01.glb"),
            ),
            (
                "meshes/Dungeons/Dwemer/Facades/DweFacadeLiftLeverLoadDown01.glb",
                Some("meshes/Dungeons/Dwemer/Facades/DweFacadeLiftLeverDown01.glb"),
            ),
            (
                "meshes/DLC02/Dungeons/IceCastle/DLC02IceDoorLoadExt.glb",
                Some("meshes/DLC02/Dungeons/IceCastle/DLC02IceDoorExt.glb"),
            ),
            // No number and no direction, just the marker before the `Door`: `LoadDoor01`.
            (
                "meshes/Dungeons/Nordic/Exterior/Animated/NorLabyrinthianDoor/\
                 NorLabyrinthianLoadDoor01.glb",
                Some(
                    "meshes/Dungeons/Nordic/Exterior/Animated/NorLabyrinthianDoor/\
                     NorLabyrinthianDoor01.glb",
                ),
            ),
            // The case-insensitive one, on the model of the install's auto-load markers: its derived
            // twin is not there, and an auto-load door has no model loaded at all.
            (
                "meshes/AutoLoadMarker01.glb",
                Some("meshes/AutoMarker01.glb"),
            ),
            // A door whose model name carries no marker has no twin.
            ("meshes/Architecture/Farmhouse/FarmhouseLDoor01.glb", None),
            ("meshes/Dungeons/Riften/RatwayHall/RiftenDoor01.glb", None),
        ] {
            assert_eq!(
                twin_model_path(model).as_deref(),
                twin,
                "{model}: the twin is the same path with the first `load` out of the file name"
            );
        }
    }

    /// The same marker rule on the names of the models' own root nodes, which is what turns a door
    /// node's id into the id the twin's clips use for it. Real root node names, read from the models
    /// themselves (`tools/research/load_door_twin_coverage.py pairs`).
    #[test]
    fn the_twin_root_is_the_model_root_with_the_marker_taken_out() {
        for (model_root, twin_root) in [
            ("DwemerLargeDoorLoad01", Some("DwemerLargeDoor01")),
            ("DwemerSmallDoorLoad01", Some("DwemerSmallDoor01")),
            ("CasExFreeSmDoorLoad01", Some("CasExFreeSmDoor01")),
            ("CasExFreeLgDoorLoad01", Some("CasExFreeLgDoor01")),
            ("NorDoorSmallLoad02", Some("NorDoorSmall02")),
            // Roots that do not follow the rule - the twin of `NorDoorSmallLoad02` is named
            // `Dummy01` - find nothing to borrow from, which is what a model whose names are
            // unrelated to its twin's should do. 6 of the install's 19 twinned pairs are like this.
            ("Dummy12", None),
            ("ImpDoorDouble01", None),
        ] {
            assert_eq!(
                strip_load_marker(model_root).as_deref(),
                twin_root,
                "{model_root}"
            );
        }

        // And the ids themselves: a node of the door's model has the id the twin gives it, with the
        // model's root written the way the twin writes it.
        let door = node_path(OWN_LEAF);
        assert_eq!(
            twin_target_ids(&door),
            vec![target_id(&[BASIS, TWIN_ROOT, OWN_LEAF])],
            "one marker-carrying element in this path, so one id the twin could have for the node"
        );
        assert!(
            !twin_target_ids(&door).contains(&target_id(&[BASIS, TWIN_ROOT, "Object45"])),
            "and it is not the twin's id for some other node"
        );
        assert!(
            twin_target_ids(&[Name::new("Creation-to-glTF basis"), Name::new(OWN_LEAF)]).is_empty(),
            "a path with no marker in it has no twin ids at all"
        );
    }

    /// The pose of `target` in `clip` at `time`, or a panic - the tests below are all about where a
    /// clip puts a leaf.
    fn pose(clip: &AnimationClip, target: AnimationTargetId, time: f32) -> Quat {
        clip.sample_clamped(animated_field!(Transform::rotation), target, time)
            .expect("the clip turns this node")
    }

    /// How far `clip` turns `target` from its first pose, at `time`, in degrees.
    fn swung_from_rest(clip: &AnimationClip, target: AnimationTargetId, time: f32) -> f32 {
        pose(clip, target, 0.0)
            .angle_between(pose(clip, target, time))
            .to_degrees()
    }

    /// A narrow clip is scaled until its widest leaf stands at [`SWUNG_OPEN_DEGREES`], about the
    /// hinges it already had: `t = 0` is the pose the door stands in and does not move, the clip
    /// keeps its length and its nodes, and only the distance it turns is different.
    #[test]
    fn a_narrow_clip_is_scaled_until_its_widest_leaf_opens_the_doorway() {
        let leaf = target_id(&[BASIS, MODEL_ROOT, OWN_LEAF]);
        let open = clip_swinging(CLIP_SECONDS, leaf, 0.0, NARROW_SWING_DEGREES);
        let factor = swing_scale(swing_degrees(&open, &HashSet::from([leaf])));
        assert!((factor - SWUNG_OPEN_DEGREES / NARROW_SWING_DEGREES).abs() < 1.0e-4);

        let (swung, close) = swung_open_clips(Some(&open), None, &HashSet::from([leaf]), factor)
            .expect("a rotation curve is what scaling is for");

        // The same length, the same node, and the same pose to start from: the swing is longer, not
        // different.
        assert_eq!(swung.duration(), open.duration());
        assert_eq!(
            swung.curves().keys().copied().collect::<HashSet<_>>(),
            open.curves().keys().copied().collect::<HashSet<_>>(),
            "the scaled clip turns the door's own nodes and no others"
        );
        let rest_drift = pose(&swung, leaf, 0.0)
            .angle_between(pose(&open, leaf, 0.0))
            .to_degrees();
        assert!(
            rest_drift < 0.01,
            "t = 0 is the pose the door stands in, and stays it: {rest_drift} degrees of drift"
        );

        // And it opens the doorway: the widest leaf stands at `SWUNG_OPEN_DEGREES` where the clip it
        // came from managed 8.
        assert!(
            (swung_from_rest(&swung, leaf, swung.duration()) - SWUNG_OPEN_DEGREES).abs() < 0.5,
            "the scaled swing: {} degrees",
            swung_from_rest(&swung, leaf, swung.duration())
        );
        assert!(
            swing_degrees(&swung, &HashSet::from([leaf])) >= DOORWAY_CLEAR_DEGREES,
            "which is a doorway, so the leaves stay drawn when the door is open"
        );
        assert!(close.is_none(), "the door's own clip has no close to scale");
    }

    /// One factor for the whole clip, from the widest leaf, so the leaves keep their motion relative
    /// to each other: a door that turns one leaf 8 degrees and the other 12 keeps its 2:3.
    #[test]
    fn the_scale_is_one_factor_for_every_leaf() {
        let first = target_id(&[BASIS, MODEL_ROOT, OWN_LEAF]);
        let second = target_id(&[BASIS, MODEL_ROOT, "Plane02"]);
        let mut open = clip_swinging(CLIP_SECONDS, first, 0.0, NARROW_SWING_DEGREES);
        open.add_curve_to_target(
            second,
            AnimatableCurve::new(
                animated_field!(Transform::rotation),
                AnimatableKeyframeCurve::new([
                    (0.0, Quat::from_rotation_y(0.0)),
                    (CLIP_SECONDS, Quat::from_rotation_y(12.0_f32.to_radians())),
                ])
                .expect("two keys at different times"),
            ),
        );
        let moved = HashSet::from([first, second]);
        let factor = swing_scale(swing_degrees(&open, &moved));

        let (swung, _) = swung_open_clips(Some(&open), None, &moved, factor).expect("two leaves");

        let widest = swung_from_rest(&swung, second, swung.duration());
        let narrowest = swung_from_rest(&swung, first, swung.duration());
        assert!(
            (widest - SWUNG_OPEN_DEGREES).abs() < 0.5,
            "the widest leaf is the one the target was taken from: {widest} degrees"
        );
        assert!(
            (narrowest / widest - NARROW_SWING_DEGREES / 12.0).abs() < 0.01,
            "and the other kept its motion relative to it: {narrowest} against {widest} degrees"
        );
    }

    /// A clip that already clears the doorway is never scaled - not up, and not down either - and
    /// neither is one that turns nothing at all, which has no rotation to scale.
    #[test]
    fn a_clip_that_already_clears_the_doorway_is_never_scaled() {
        assert_eq!(swing_scale(WIDE_SWING_DEGREES), 1.0);
        assert_eq!(swing_scale(DOORWAY_CLEAR_DEGREES), 1.0);
        assert_eq!(
            swing_scale(0.0),
            1.0,
            "a clip that turns nothing has nothing to scale"
        );
        assert_eq!(
            swing_scale(f32::NAN),
            1.0,
            "and neither has one whose swing cannot be measured"
        );

        // And a door whose own clip turns its leaf 120 degrees keeps that clip: it is not the door's
        // swing that is wrong, and scaling it would be inventing motion just as surely.
        let mut app = door_app();
        let model = door_with_model(&mut app, WIDE_SWING_DEGREES);
        app.update();
        let animation = *app
            .world()
            .get::<DoorAnimation>(model.door)
            .expect("a resolved door");
        assert!(animation.clears_doorway);
        assert!(
            app.world().resource::<AdjustedSwings>().0.is_empty(),
            "nothing was done to this door's swing at all"
        );
    }

    /// A clip that barely moves at all is not blown up past its own geometry: the widest leaf is
    /// taken to `SWUNG_OPEN_DEGREES` when the clip's swing is close enough for the factor to stay
    /// under [`MAX_SWING_SCALE`], and stops at the cap when it is not.
    #[test]
    fn the_scale_factor_is_capped() {
        // `RiftenRWDoorLoad01` turns its leaf 2.9 degrees: 31 times its own swing would be 90, and
        // 20 times it is 58 - still a doorway, still this door's own swing.
        let factor = swing_scale(2.9);
        assert_eq!(factor, MAX_SWING_SCALE, "capped");

        let leaf = target_id(&[BASIS, MODEL_ROOT, OWN_LEAF]);
        let open = clip_swinging(CLIP_SECONDS, leaf, 0.0, 2.9);
        let moved = HashSet::from([leaf]);
        let (swung, _) = swung_open_clips(Some(&open), None, &moved, factor).expect("one leaf");
        let degrees = swung_from_rest(&swung, leaf, swung.duration());
        assert!(
            (degrees - 2.9 * MAX_SWING_SCALE).abs() < 0.5,
            "the cap's swing: {degrees} degrees"
        );
        assert!(
            degrees >= DOORWAY_CLEAR_DEGREES,
            "which still opens the doorway: {degrees} degrees"
        );

        // The demo route's own door, whose leaves turn 5.36 and 8.74 degrees: the widest leaf sets
        // the factor, and it is nowhere near the cap. This is the number the report quotes.
        let factor = swing_scale(8.74);
        assert!(
            (factor - 10.30).abs() < 0.01,
            "the demo route's `DweDoorLarge01Load` is scaled by {factor}"
        );
    }

    /// A door whose `Close` is its `Open` run backwards - every load door in the install that has
    /// both - closes by retracing the scaled swing, not by running `Close` through the same scaling
    /// and carrying the leaf past closed.
    #[test]
    fn a_mirrored_close_is_the_scaled_open_run_backwards() {
        let leaf = target_id(&[BASIS, MODEL_ROOT, OWN_LEAF]);
        let open = clip_swinging(CLIP_SECONDS, leaf, 0.0, NARROW_SWING_DEGREES);
        let close = clip_swinging(CLIP_SECONDS, leaf, NARROW_SWING_DEGREES, 0.0);
        let moved = HashSet::from([leaf]);
        assert!(
            close_mirrors_open(&open, &close, &moved),
            "the fixture's close is the open run backwards"
        );

        let factor = swing_scale(swing_degrees(&open, &moved));
        let (swung, closed) = swung_open_clips(Some(&open), Some(&close), &moved, factor)
            .expect("a swinging door with a mirror close");
        let closed = closed.expect("the close came back");

        assert_eq!(
            closed.duration(),
            swung.duration(),
            "a reversed clip is the same length as the one it came from"
        );
        // Closing starts where the swing ended and ends where the door stands.
        let from = swung_from_rest(&swung, leaf, swung.duration());
        assert!(
            (from - SWUNG_OPEN_DEGREES).abs() < 0.5,
            "the scaled swing opens to {from} degrees"
        );
        let closing = pose(&closed, leaf, 0.0)
            .angle_between(pose(&swung, leaf, swung.duration()))
            .to_degrees();
        assert!(
            closing < 0.5,
            "closing starts from the pose the swing ended on, not {closing} degrees from it"
        );
        let stopped = pose(&closed, leaf, closed.duration())
            .angle_between(pose(&open, leaf, 0.0))
            .to_degrees();
        assert!(
            stopped < 0.5,
            "and ends where the door stands, not {stopped} degrees from it"
        );

        // Every sample of the close is a sample of the swing, taken from the other end: closing
        // retraces exactly the motion opening made.
        for step in 0..=10 {
            let fraction = step as f32 / 10.0;
            let closing_pose = pose(&closed, leaf, closed.duration() * fraction);
            let opening_pose = pose(&swung, leaf, swung.duration() * (1.0 - fraction));
            let apart = closing_pose.angle_between(opening_pose).to_degrees();
            assert!(
                apart < 0.5,
                "at {fraction} of the way back the door is {apart} degrees from where it was on \
                 the way out"
            );
        }
    }

    /// A `Close` that is a motion of its own rather than the `Open` run backwards is scaled the same
    /// way `Open` is - about its own first pose - because there is no swing of its own to retrace.
    #[test]
    fn a_close_that_is_a_motion_of_its_own_is_scaled_its_own_way() {
        let leaf = target_id(&[BASIS, MODEL_ROOT, OWN_LEAF]);
        let open = clip_swinging(CLIP_SECONDS, leaf, 0.0, NARROW_SWING_DEGREES);
        // Starts 5 degrees off where the swing ended, and ends elsewhere than where it started:
        // not this door's opening run backwards.
        let close = clip_swinging(CLIP_SECONDS, leaf, 5.0, 0.0);
        let moved = HashSet::from([leaf]);
        assert!(
            !close_mirrors_open(&open, &close, &moved),
            "5 degrees is not where the swing ended at 8"
        );

        let factor = swing_scale(swing_degrees(&open, &moved));
        let (_, closed) = swung_open_clips(Some(&open), Some(&close), &moved, factor)
            .expect("a swinging door with a close of its own");
        let closed = closed.expect("the close came back");

        let start = swung_from_rest(&closed, leaf, 0.0);
        assert!(
            start < 0.01,
            "a scaled clip starts where its own clip started: {start} degrees of drift"
        );
        // Its own motion - 5 degrees down to 0 - turned the same way and by the same factor.
        let turned = swung_from_rest(&closed, leaf, closed.duration());
        let expected = 5.0 * factor;
        assert!(
            (turned - expected).abs() < 0.5,
            "the close's own 5 degrees became {turned}, not {expected}"
        );
    }

    /// A door whose own clip is too narrow to open the doorway is given its own swing, scaled: the
    /// leaves it turns are the leaves of its own model, they stay drawn once the door is open, and
    /// the door takes exactly as long about it as its own clip did.
    #[test]
    fn a_door_whose_own_clip_barely_moves_is_given_a_swing_of_its_own() {
        let mut app = door_app();
        let model = door_with_model(&mut app, NARROW_SWING_DEGREES);

        app.update();

        let animation = *app
            .world()
            .get::<DoorAnimation>(model.door)
            .expect("a resolved door");
        assert!(
            animation.clears_doorway,
            "the door's own 8 degrees scaled to open the doorway: {animation:?}"
        );
        assert_eq!(
            app.world()
                .resource::<AdjustedSwings>()
                .0
                .get(PLAIN_MODEL_PATH),
            Some(&SwingSource::Scaled)
        );
        assert!(
            app.world().get::<DoorLeaf>(model.moved).is_some(),
            "the leaf the scaled clip turns is the node its own clip turned"
        );

        activate(&mut app, model.door);
        step(&mut app, 4);
        let part_way = leaf_degrees(&app, model.moved);
        assert!(
            (0.0..=WIDE_SWING_DEGREES).contains(&part_way),
            "the swing is under way: {part_way} degrees"
        );

        step(&mut app, 8);
        assert_eq!(state(&app, model.door), DoorState::Open { animated: true });
        let degrees = leaf_degrees(&app, model.moved);
        assert!(
            (degrees - SWUNG_OPEN_DEGREES).abs() < 1.0,
            "and it ends standing open: {degrees} degrees"
        );
        assert_ne!(
            leaf_visibility(&app, model.moved),
            Visibility::Hidden,
            "a door that swings open keeps its leaf"
        );
    }
}
