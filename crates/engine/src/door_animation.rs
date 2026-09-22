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
//! `app.run` adds the plugin for interactive runs, next to `PortalPlugin`:
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
    transition::{CrossingHeld, OpenDoor},
    world::components::MeshHandle,
};
use bevy::{
    animation::{
        AnimationClip, AnimationPlayer, AnimationTargetId, animated_field,
        graph::{AnimationGraph, AnimationGraphHandle, AnimationNodeIndex},
        transition::AnimationTransitions,
    },
    prelude::*,
};
use std::collections::HashSet;
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
#[derive(Component, Debug, Clone)]
struct PendingDoorModel {
    /// The model's root `Gltf` asset.
    model: Handle<Gltf>,
    /// Frames spent waiting for the model's clip assets and the scene's [`AnimationPlayer`] since
    /// the model itself loaded. A door that waits longer than [`SCENE_WAIT_FRAMES`] is given up on.
    waiting: u32,
}

/// What an animating door model turned out to be: the clips to play, the entity the loader put the
/// door's [`AnimationPlayer`] on, and the nodes the clips move.
#[derive(Debug)]
struct ResolvedModel {
    player: Entity,
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
/// Add it for interactive runs, next to `PortalPlugin`. It needs nothing but the asset server: a
/// run that never writes [`OpenDoor`] never changes a door, so a `--shots` run is unaffected.
pub struct DoorAnimationPlugin;

impl Plugin for DoorAnimationPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<OpenDoor>().add_systems(
            Update,
            (
                request_door_models,
                attach_door_animations,
                activate_doors,
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
            commands.entity(entity).insert((
                DoorState::Open { animated: false },
                DoorAnimation::default(),
            ));
            continue;
        }
        commands.entity(entity).insert(DoorState::Closed);
        match model {
            Some(MeshHandle(path)) => {
                commands.entity(entity).insert(PendingDoorModel {
                    model: asset_server.load(path.clone()),
                    waiting: 0,
                });
            }
            // Nothing to animate: a load door whose base has no model is a static door.
            None => {
                commands.entity(entity).insert(DoorAnimation::default());
            }
        }
    }
}

/// Reads the clips off a door's model and hands the door its own animation graph.
///
/// The graph goes on the entity the loader gave the [`AnimationPlayer`]
/// (`bevy_gltf-0.19.0/src/loader/mod.rs#L1093` inserts the player and no graph), along with the
/// [`AnimationTransitions`] that fade one clip into the other when the door reverses. The graph's
/// node indices are what the state machine plays, and the nodes the clips move are marked
/// [`DoorLeaf`] here, because this is the only place that has both the clips and the spawned scene
/// in hand.
///
/// Everything this needs arrives a frame or more after the reference is spawned - the model asset,
/// its clip sub-assets, the scene's nodes - so a door that cannot be finished yet is left pending
/// and tried again next frame, up to [`SCENE_WAIT_FRAMES`].
#[allow(clippy::too_many_arguments)]
fn attach_door_animations(
    mut commands: Commands,
    models: Res<Assets<Gltf>>,
    clips: Res<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut doors: Query<(Entity, &LoadDoor, &mut PendingDoorModel), Without<DoorAnimation>>,
    children: Query<&Children>,
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
            // The model names no clips at all: a static leaf, and nothing left to wait for.
            commands
                .entity(door)
                .insert(DoorAnimation::default())
                .remove::<PendingDoorModel>();
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
                .insert(DoorAnimation::default())
                .remove::<PendingDoorModel>();
            continue;
        };

        let mut clip_handles = vec![open.clone()];
        if let Some(close) = &close {
            clip_handles.push(close.clone());
        }
        let (graph, nodes) = AnimationGraph::from_clips(clip_handles);
        let graph = graphs.add(graph);
        let close_seconds = resolved.close_seconds;
        let open_seconds = resolved.open_seconds;

        mark_leaf_nodes(&mut commands, door, &resolved.moved, &children, &targets);

        commands.entity(resolved.player).insert((
            AnimationGraphHandle(graph.clone()),
            AnimationTransitions::new(),
        ));
        commands
            .entity(door)
            .insert(DoorAnimation {
                player: Some(resolved.player),
                open: Some(DoorClip {
                    node: nodes[0],
                    seconds: open_seconds,
                }),
                close: close.map(|_| DoorClip {
                    node: nodes[1],
                    seconds: close_seconds.unwrap_or(0.0),
                }),
                clears_doorway: resolved.open_swing >= DOORWAY_CLEAR_DEGREES,
            })
            .remove::<PendingDoorModel>();
    }
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
        open_seconds: open_clip.duration(),
        open_swing: swing_degrees(open_clip, &moved),
        close_seconds: close_clip.map(AnimationClip::duration),
        moved,
    })
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
/// A finished clip counts as the whole way through, and so does a clip of no length: a model that
/// exported a zero-second sequence would otherwise leave its door mid-swing for good.
fn clip_fraction(player: &AnimationPlayer, clip: DoorClip) -> Option<f32> {
    let animation = player.animation(clip.node)?;
    if animation.is_finished() || clip.seconds <= 0.0 {
        return Some(1.0);
    }
    Some(animation.elapsed() / clip.seconds)
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
            commands.entity(node).insert(DoorLeaf { door });
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
            .spawn((
                load_door(false),
                DoorState::Closed,
                PendingDoorModel { model, waiting: 0 },
            ))
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

    /// Whether a door's leaves are hidden when it opens is a fact about the `Open` clip - how far it
    /// turns them - so it is read off the clip at the moment the door is given its animation, not
    /// guessed from the state or watched while the door swings.
    #[test]
    fn the_clip_tells_the_door_whether_its_leaves_clear_the_doorway() {
        for (swing, clears) in [
            (NARROW_SWING_DEGREES, false),
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
            .spawn((
                load_door(false),
                DoorState::Closed,
                PendingDoorModel { model, waiting: 0 },
            ))
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
            .spawn((
                load_door(false),
                DoorState::Closed,
                PendingDoorModel { model, waiting: 0 },
            ))
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
}
