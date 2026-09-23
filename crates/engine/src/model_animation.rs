//! A model's own looping clip, played on every ordinary reference of it.
//!
//! Skyrim's models carry their ambient motion in the controller sequences inside the NIF - the lumber
//! mill's water wheel turning on its hub, a pile of logs shedding dust - and the converter bakes every
//! one of them into glTF animation clips on the model's own `.glb`, under the name the NIF gave the
//! sequence (`crates/converter/src/nif_animation.rs` reads a `NiControllerSequence`'s name out of the
//! NIF's string table and writes it into the document's `animations[].name` unchanged, so the names
//! are the models' own, case and all). Until now the engine played none of them: a load door's `Open`
//! and `Close` were the only clips anything looked for ([`crate::door_animation`]), and everything
//! else that was built to move stood still.
//!
//! This module plays the clip a model names in [`IDLE_CLIP_NAMES`] - `Idle` - on every reference of
//! that model that is not a door, looping forever. `Lumbermill01WaterWheel01.glb`, the model the
//! Riverwood demo's finale is a view across the river of, is exactly this: one clip named `Idle`, one
//! rotation channel, on the node chain
//! `Lumbermill01WaterWheel01 -> SawWaterWheel03 -> L1_SawWaterWheelHub`. Nothing drove it, so the
//! wheel was a still photograph of a working mill.
//!
//! # A model is resolved once, a reference is started once
//!
//! The question this module asks - does this model carry an `Idle` clip, and how long is it - is a
//! fact about the **model**, not about the reference: a village has dozens of copies of one water
//! wheel and they all want the same clip. So the answer is resolved once per model path and kept in
//! [`IdleModelCache`], along with the one [`AnimationGraph`] every reference of that model plays it
//! through. A model with no clip of a name in [`IDLE_CLIP_NAMES`] is [`IdleModel::NoClip`] in the
//! cache and is never looked at again - which is the answer for very nearly every model in the world,
//! since a door's `Open` and a chair's nothing at all are the common case.
//!
//! On the reference side there is one thing to wait for: the glTF loader gives the spawned scene an
//! [`AnimationPlayer`] but no graph, and the player belongs to the scene, which the loader spawns a
//! frame or more after the reference itself ([`crate::streaming`]). A reference is therefore claimed
//! exactly once - it ends up with its [`PendingIdleModel`], or with an [`IdleAnimation`] that plays
//! nothing if its model has no clip - and started once its scene has produced a player: the same
//! shape as a door's own resolution in [`crate::door_animation`], with the same patience
//! ([`SCENE_WAIT_FRAMES`]) before a reference that never gets one is left as the static model it was.
//!
//! # Two copies of one model are not in lockstep
//!
//! Every reference of a model starts its copy at its own time into the clip
//! ([`start_offset`]: the reference's FormID modulo the clip's length), because a mill's two wheel
//! models turning in perfect step look like one machine drawn twice. The phase is deliberately a
//! function of the FormID and nothing else - not the entity, not the frame, not a random number -
//! because it has to be the same phase every run: the reference-shot captures compare renders against
//! earlier runs, and a phase drawn from anything else would put the wheel somewhere new each time.
//!
//! # What wires this module up
//!
//! `app.run` adds the plugin for interactive runs, next to `DoorAnimationPlugin`:
//!
//! ```ignore
//! app.add_plugins(crate::model_animation::ModelAnimationPlugin);
//! ```
//!
//! and `lib.rs` declares the module:
//!
//! ```ignore
//! pub mod model_animation;
//! ```
//!
//! Nothing else reads this module: it drives the clips itself and asks nothing of any other plugin.

use crate::{
    doors::LoadDoor,
    world::components::{FormId, MeshHandle},
};
use bevy::{
    animation::{
        AnimationClip, AnimationPlayer,
        graph::{AnimationGraph, AnimationGraphHandle, AnimationNodeIndex},
    },
    prelude::*,
};
use std::collections::HashMap;

/// The clip names this plugin will play, in preference order. A model's first match wins.
pub const IDLE_CLIP_NAMES: [&str; 1] = ["Idle"];

/// How many frames a reference waits for the [`AnimationPlayer`] of its scene before giving up and
/// staying still.
///
/// The model, its clip sub-assets and the scene the loader spawns are the same file, and they arrive
/// within a frame or two of each other, so a scene that has produced no player two seconds later is
/// a scene that never will be - a model whose clips moved no node the loader could find. A reference
/// that gives up is the static model it was before this module, which is what the wheel was anyway.
///
/// The same number, and the same reasoning, as `door_animation.rs`'s `SCENE_WAIT_FRAMES`.
const SCENE_WAIT_FRAMES: u32 = 120;

/// Plays a model's own looping ambient clip on references that are not doors.
///
/// Add it for interactive runs, next to `PortalPlugin` and `DoorAnimationPlugin`. It needs nothing
/// but the asset server, and a run in which no cell streams in never starts a clip.
pub struct ModelAnimationPlugin;

impl Plugin for ModelAnimationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<IdleModelCache>().add_systems(
            Update,
            (request_idle_models, resolve_idle_models, start_idle_clips).chain(),
        );
    }
}

/// What the engine did with a reference's model: the clip that is playing on it, or that its model
/// has none.
///
/// It is the marker that keeps a reference from being claimed twice, and the answer to "why is that
/// wheel still": a reference with `player: None` is one whose model named no clip this plugin plays,
/// or one whose scene never produced a player.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq)]
pub struct IdleAnimation {
    /// The entity the loader put the reference's [`AnimationPlayer`] on - a descendant of the
    /// reference, the animation root of the scene it spawned - once the clip is playing on it.
    pub player: Option<Entity>,
    /// The time, in seconds, into the clip this reference's copy started from
    /// ([`start_offset`]): the phase that keeps two copies of one model out of lockstep.
    pub offset: f32,
}

/// The loop a model plays: the graph that plays it, the node in that graph, and the clip's length.
#[derive(Debug, Clone, PartialEq)]
pub struct IdleLoop {
    /// The animation graph holding the model's one clip. Every reference of the model shares it,
    /// and the glTF loader's spawned players have none of their own - `advance_animations` needs the
    /// graph and the [`AnimationPlayer`] on the same entity (`bevy_animation-0.19.0/src/lib.rs#L1035`).
    pub graph: Handle<AnimationGraph>,
    /// The graph node [`AnimationPlayer::play`] takes to play the clip.
    pub node: AnimationNodeIndex,
    /// The clip's length in seconds ([`AnimationClip::duration`]), the modulus of [`start_offset`].
    pub seconds: f32,
}

/// What a model came to, as [`IdleModelCache`] holds it.
#[derive(Debug, Clone, PartialEq)]
pub enum IdleModel {
    /// The model names a clip this plugin plays: one graph, and the length of the clip in it.
    Plays(IdleLoop),
    /// The model names no clip this plugin plays, and it never will: the answer for most of the
    /// world's models, and a final one - a model is looked at once.
    NoClip,
}

/// What each model path resolved to, so that a model is resolved once however many references of it
/// the world holds.
///
/// A path that is not in it has not been resolved yet (its model, or the clip's own keys, are still
/// loading). Read it from another module the way this one does, through [`IdleModelCache::get`].
#[derive(Resource, Debug, Default)]
pub struct IdleModelCache(HashMap<String, IdleModel>);

impl IdleModelCache {
    /// What the model at `path` came to, or `None` while it is still being resolved.
    pub fn get(&self, path: &str) -> Option<&IdleModel> {
        self.0.get(path)
    }
}

/// A reference that is waiting for its model, or for its scene's [`AnimationPlayer`], before the
/// clip can be started on it.
///
/// It is the claim on the reference: an entity carrying it is one this module is part way through,
/// and it carries the model path the answer is cached under - kept for the frame the model does
/// arrive, and for the log line when one never does.
#[derive(Component, Debug)]
struct PendingIdleModel {
    /// The model's root `Gltf` asset: the thing the named clips are sub-assets of.
    model: Handle<Gltf>,
    /// The path the model was loaded from, which is the cache key. Kept rather than read back off
    /// the [`MeshHandle`], because a reference may carry no mesh by the time this is looked at.
    path: String,
    /// The clip to play, once the model has resolved to one: `None` while the model itself is still
    /// on its way.
    clip: Option<IdleLoop>,
    /// Frames spent waiting for the scene's player since the clip was resolved. A reference that
    /// waits [`SCENE_WAIT_FRAMES`] of them gives up and stays still.
    waiting: u32,
}

/// An ordinary reference whose model has not been looked up yet: one with a model, and not a load
/// door.
///
/// ["DoorAnimation"](crate::door_animation) is the door module's own and private; every entity that
/// carries one also carries a [`LoadDoor`] (`door_animation.rs`'s `request_door_models` queries
/// `&LoadDoor`), so excluding the door is excluding both.
type UnclaimedReferenceQuery<'world, 'state> = Query<
    'world,
    'state,
    (Entity, &'static MeshHandle, &'static FormId),
    (
        Without<LoadDoor>,
        Without<IdleAnimation>,
        Without<PendingIdleModel>,
    ),
>;

/// A reference this module has claimed and is part way through.
type PendingIdleQuery<'world, 'state> = Query<
    'world,
    'state,
    (Entity, &'static FormId, &'static mut PendingIdleModel),
    Without<IdleAnimation>,
>;

/// Claims every ordinary reference for its model's clip.
///
/// A model whose answer is already in the cache - which is every model after the first reference of
/// it - is settled here and now, and only a reference whose model has not resolved yet is left
/// [`PendingIdleModel`] for the frames the model takes to load. The alternative, letting every
/// reference wait a turn for [`resolve_idle_models`] to read the cache, would move two components
/// per reference on the frame a cell is spawned, which is the frame streaming is measured on.
fn request_idle_models(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    cache: Res<IdleModelCache>,
    references: UnclaimedReferenceQuery,
) {
    for (reference, MeshHandle(path), _) in &references {
        let clip = match cache.get(path) {
            // The model names no clip this plugin plays: the reference is done with, and neither it
            // nor the model will be asked again.
            Some(IdleModel::NoClip) => {
                commands.entity(reference).insert(IdleAnimation::default());
                continue;
            }
            // The model has one, already resolved: this reference waits only for its own scene.
            Some(IdleModel::Plays(loop_)) => Some(loop_.clone()),
            // Nothing is known about the model yet.
            None => None,
        };
        commands.entity(reference).insert(PendingIdleModel {
            model: asset_server.load(path.clone()),
            path: path.clone(),
            clip,
            waiting: 0,
        });
    }
}

/// Resolves the models the references are waiting for, one model path at a time.
///
/// A model with no clip this plugin plays is finished with for good: the reference is given an empty
/// [`IdleAnimation`] and nothing about it is looked at again. A model with one hands every reference
/// the same [`IdleLoop`], which is the graph and the clip's length.
fn resolve_idle_models(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    models: Res<Assets<Gltf>>,
    clips: Res<Assets<AnimationClip>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut cache: ResMut<IdleModelCache>,
    mut waiting: Query<(Entity, &mut PendingIdleModel), Without<IdleAnimation>>,
) {
    for (reference, mut pending) in &mut waiting {
        if pending.clip.is_some() {
            continue;
        }
        let resolved = resolve_idle_model(
            &mut cache,
            &models,
            &clips,
            &mut graphs,
            &asset_server,
            &pending.model,
            &pending.path,
        );
        match resolved {
            Some(IdleModel::Plays(loop_)) => {
                pending.clip = Some(loop_);
                // The clip is the thing the scene wait is about, so it starts when the clip does.
                pending.waiting = 0;
            }
            Some(IdleModel::NoClip) => {
                commands
                    .entity(reference)
                    .insert(IdleAnimation::default())
                    .remove::<PendingIdleModel>();
            }
            // The model, or the clip it names, is still on its way.
            None => {}
        }
    }
}

/// Starts the clip on every reference whose model has one and whose scene has arrived.
///
/// The graph goes on the entity the loader gave the [`AnimationPlayer`] (it gives it none), and the
/// clip is played looping from the phase the reference's FormID asks for. A reference whose scene
/// never produces a player - a model whose clips moved no node the loader could find - is given
/// [`SCENE_WAIT_FRAMES`] frames and then left as the static model it is.
fn start_idle_clips(
    mut commands: Commands,
    mut pending: PendingIdleQuery,
    children: Query<&Children>,
    mut players: Query<&mut AnimationPlayer>,
) {
    for (reference, form_id, mut waiting) in &mut pending {
        let Some(loop_) = waiting.clip.clone() else {
            // The model has not resolved yet: `resolve_idle_models` owns that wait, and it has no
            // scene to look for until it is over.
            continue;
        };
        let Some(player) = std::iter::once(reference)
            .chain(children.iter_descendants(reference))
            .find(|entity| players.contains(*entity))
        else {
            waiting.waiting += 1;
            if waiting.waiting < SCENE_WAIT_FRAMES {
                continue;
            }
            debug!(
                model = %waiting.path,
                "no animation player in the model's scene after {SCENE_WAIT_FRAMES} frames; \
                 its references stay still"
            );
            commands
                .entity(reference)
                .insert(IdleAnimation::default())
                .remove::<PendingIdleModel>();
            continue;
        };

        let offset = start_offset(form_id.0, loop_.seconds);
        {
            let Ok(mut playing) = players.get_mut(player) else {
                // The player was despawned between the scan above and here, which means its cell
                // went with it: there is nothing left to start.
                continue;
            };
            // Looping, and from this reference's own phase into the clip. `set_seek_time` rather
            // than `seek_to`: the part of the clip that is being skipped should not fire its events
            // (the same note as `door_animation.rs`'s own `play_clip`).
            playing.play(loop_.node).repeat().set_seek_time(offset);
        }
        // The graph goes on the entity the loader gave the player: `advance_animations` needs the
        // graph and the player together, and the loader gives a spawned scene no graph at all.
        commands
            .entity(player)
            .insert(AnimationGraphHandle(loop_.graph.clone()));

        commands
            .entity(reference)
            .insert(IdleAnimation {
                player: Some(player),
                offset,
            })
            .remove::<PendingIdleModel>();
    }
}

/// The idle clip of a model, resolved once per model path and cached - `None` while the model, or
/// the clip's own keys, are still loading.
///
/// Everything this module needs is a fact about the model rather than about the reference: whether
/// it names a clip in [`IDLE_CLIP_NAMES`], how long that clip is, and the one graph that plays it.
/// Most models in the world name nothing this plugin plays, and [`IdleModel::NoClip`] is cached for
/// them, so the question is asked once per model path and never again.
fn resolve_idle_model(
    cache: &mut IdleModelCache,
    models: &Assets<Gltf>,
    clips: &Assets<AnimationClip>,
    graphs: &mut Assets<AnimationGraph>,
    asset_server: &AssetServer,
    model: &Handle<Gltf>,
    path: &str,
) -> Option<IdleModel> {
    if let Some(resolved) = cache.get(path) {
        return Some(resolved.clone());
    }
    let Some(loaded) = models.get(model) else {
        // A model whose load has failed will never name a clip, and a reference waiting for one would
        // wait for good - so the failure is the same answer as a model with no clip at all.
        if asset_server.load_state(model).is_failed() {
            debug!(model = %path, "the model failed to load; its references have no idle clip");
            cache.0.insert(path.to_owned(), IdleModel::NoClip);
            return Some(IdleModel::NoClip);
        }
        return None;
    };
    let Some((name, clip)) = idle_clip(loaded) else {
        cache.0.insert(path.to_owned(), IdleModel::NoClip);
        return Some(IdleModel::NoClip);
    };
    // The clip's name is in the model's document; its keys are a sub-asset loaded beside it, and
    // until they are here there is no length to resolve one way or the other.
    let seconds = clips.get(clip).map(AnimationClip::duration)?;
    // A clip of no length is a clip there is nothing to play, and one Bevy's own tick cannot hold:
    // `ActiveAnimation::update` takes the seek time modulo the clip's length
    // (`bevy_animation-0.19.0/src/lib.rs#L586`), which is a NaN for a length of zero.
    if seconds <= 0.0 || !seconds.is_finite() {
        debug!(
            model = %path,
            clip = %name,
            "the model's clip has no length; its references stay still"
        );
        cache.0.insert(path.to_owned(), IdleModel::NoClip);
        return Some(IdleModel::NoClip);
    }
    let (graph, nodes) = AnimationGraph::from_clips([clip.clone()]);
    let loop_ = IdleLoop {
        graph: graphs.add(graph),
        node: nodes[0],
        seconds,
    };
    debug!(
        model = %path,
        clip = %name,
        seconds = loop_.seconds,
        "the model's own idle clip plays on every reference of it"
    );
    cache
        .0
        .insert(path.to_owned(), IdleModel::Plays(loop_.clone()));
    Some(IdleModel::Plays(loop_))
}

/// The clip a model plays, if it names one of [`IDLE_CLIP_NAMES`]: the name the model itself gives
/// it, and its handle.
///
/// Exact, case and all. The converter copies a sequence's own name out of the NIF and into the glTF
/// document (`crates/converter/src/nif_animation.rs`), and the loader keeps it, so the names this
/// matches are the models' own: `Idle` is a clip, `idle` is a different clip that a model may have
/// instead, and a model that has only that one is a model this plugin leaves alone.
fn idle_clip(model: &Gltf) -> Option<(&str, &Handle<AnimationClip>)> {
    IDLE_CLIP_NAMES.iter().find_map(|wanted| {
        model
            .named_animations
            .iter()
            .find(|(name, _)| name.as_ref() == *wanted)
            .map(|(name, clip)| (name.as_ref(), clip))
    })
}

/// The time into its clip that a reference of this FormID starts from: the FormID taken modulo the
/// clip's length.
///
/// This is deliberately the FormID and nothing else. The phase has to be a fact about the data,
/// because the reference-shot captures compare renders byte for byte against earlier runs: a wheel
/// started from the frame count, the entity or a random number would be somewhere new every time.
/// Two copies of one model whose FormIDs differ land at different times into the clip - which is the
/// whole point, since a mill's two wheels turning in perfect step look like one machine drawn twice.
///
/// A clip of no length has no phase to take, and a FormID with a clip shorter than a second takes
/// one in whole seconds: a clip of no length is never played at all ([`resolve_idle_model`]), and
/// this zero is what keeps a remainder by zero - a NaN - out of the seek time beside it.
fn start_offset(form_id: u32, seconds: f32) -> f32 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0.0;
    }
    // In `f64`, so that a whole `u32` FormID keeps its low bits: the remainder lands in
    // `[0, seconds)` for any pair of them, and the check below is for the rounding back to `f32`
    // that a clip of an absurd length could produce.
    let offset = (f64::from(form_id) % f64::from(seconds)) as f32;
    if offset.is_finite() && offset < seconds {
        offset
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        animation::{AnimationTargetId, RepeatAnimation},
        asset::AssetApp,
        time::TimeUpdateStrategy,
    };
    use std::{collections::HashSet, time::Duration};

    /// The clip name a model's ambient loop is expected to carry.
    const IDLE: &str = "Idle";

    /// A model path for tests that need a reference with one, in the shape the converter writes.
    const WATER_WHEEL: &str = "meshes/architecture/farmhouse/lumbermill01waterwheel01.glb";

    /// A model path for a model with no clip this plugin plays (`MillLogPile.glb` carries
    /// `LoadDust` and `PileDust`, and no `Idle`).
    const LOG_PILE: &str = "meshes/furniture/clutter/milllogpile.glb";

    /// A reference app with no window, no renderer and no GPU: the animation asset collections, the
    /// one asset type this module reads handles of, and this module.
    ///
    /// The clock does not advance, so a clip's seek time is exactly the phase its reference was
    /// started from and the assertions can be about the phase rather than about a frame's worth of
    /// drift.
    fn idle_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            bevy::animation::AnimationPlugin,
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO))
        .init_asset::<Gltf>()
        .add_plugins(ModelAnimationPlugin);
        app
    }

    /// A reference with a model, as [`crate::streaming`] spawns one: the mesh handle this module
    /// reads, and the FormID the phase comes from.
    fn reference(app: &mut App, path: &str, form_id: u32) -> Entity {
        app.world_mut()
            .spawn((
                Name::new(format!("Reference {form_id:08X}")),
                MeshHandle(path.to_owned()),
                FormId(form_id),
            ))
            .id()
    }

    /// The scene the loader spawns under a reference: an animation root carrying the
    /// [`AnimationPlayer`] it gives it, and no graph, which is what this module adds.
    fn scene(app: &mut App, reference: Entity) -> Entity {
        app.world_mut()
            .spawn((
                Name::new("Lumbermill01WaterWheel01"),
                AnimationPlayer::default(),
                AnimationTargetId::from_name(&Name::new("Lumbermill01WaterWheel01")),
                ChildOf(reference),
            ))
            .id()
    }

    /// A `Gltf` asset carrying the clips `clips` names, added the way the loader leaves one: the
    /// model's own root asset, with its clips as named sub-assets of it.
    fn model(app: &mut App, clips: &[(&str, f32)]) -> Handle<Gltf> {
        let named: Vec<(Box<str>, Handle<AnimationClip>)> = clips
            .iter()
            .map(|(name, seconds)| {
                let mut clip = AnimationClip::default();
                clip.set_duration(*seconds);
                (
                    Box::from(*name),
                    app.world_mut()
                        .resource_mut::<Assets<AnimationClip>>()
                        .add(clip),
                )
            })
            .collect();
        let animations = named.iter().map(|(_, clip)| clip.clone()).collect();
        app.world_mut().resource_mut::<Assets<Gltf>>().add(Gltf {
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
            animations,
            named_animations: named.into_iter().collect(),
            source: None,
        })
    }

    /// A reference waiting for its model, the way `request_idle_models` leaves one whose model has
    /// not loaded yet - or, with a clip, one that is waiting for its scene.
    fn pending(
        app: &mut App,
        reference: Entity,
        path: &str,
        model: Handle<Gltf>,
        clip: Option<IdleLoop>,
    ) {
        app.world_mut()
            .entity_mut(reference)
            .insert(PendingIdleModel {
                model,
                path: path.to_owned(),
                clip,
                waiting: 0,
            });
    }

    /// `frames` frames.
    fn step(app: &mut App, frames: u32) {
        for _ in 0..frames {
            app.update();
        }
    }

    fn idle(app: &App, reference: Entity) -> IdleAnimation {
        *app.world()
            .get::<IdleAnimation>(reference)
            .expect("the reference carries the answer")
    }

    fn cache(app: &App) -> &IdleModelCache {
        app.world().resource::<IdleModelCache>()
    }

    fn player(app: &App, entity: Entity) -> &AnimationPlayer {
        app.world()
            .get::<AnimationPlayer>(entity)
            .expect("the scene's player")
    }

    /// The model asset the handle names, as a test asking about the clip match needs it.
    fn gltf<'a>(app: &'a App, model: &Handle<Gltf>) -> &'a Gltf {
        app.world()
            .resource::<Assets<Gltf>>()
            .get(model)
            .expect("the test added the model itself")
    }

    /// The wheel the Riverwood finale is a view of, as the converter actually wrote it: it carries
    /// one clip, and the clip's own name is `Idle`, which is the name this plugin plays.
    ///
    /// The name is the whole premise of the feature. The converter copies a `NiControllerSequence`'s
    /// name from the NIF into the glTF document (`crates/converter/src/nif_animation.rs`) and the
    /// loader keys the model's `named_animations` by that same string, so a wheel whose sequence
    /// were named anything else - or a converter that renamed them - would leave the mill turning
    /// nothing and nothing in the engine would say so. This test reads the document the way
    /// `door_animation.rs` reads the route's door models.
    ///
    /// The converted assets are game data and are never committed, so this test is opt-in: it is
    /// `#[ignore]`d and skips - printing why - when `OPENSKYRIM_CONVERTED_DIR` does not name a
    /// converted asset tree, so CI never needs proprietary data (ADR-0002).
    #[test]
    #[ignore = "reads the converted Skyrim models (OPENSKYRIM_CONVERTED_DIR)"]
    fn the_wheel_names_its_clip_idle() {
        let Some(assets) =
            std::env::var_os("OPENSKYRIM_CONVERTED_DIR").map(std::path::PathBuf::from)
        else {
            eprintln!("skipping: set OPENSKYRIM_CONVERTED_DIR to a converted asset tree");
            return;
        };
        const WHEEL: &str = "meshes/architecture/farmhouse/lumbermill01waterwheel01.glb";
        let path = assets.join(WHEEL);
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipping: {} is not installed", path.display());
            return;
        };
        let document = bevy::gltf::gltf::Gltf::from_slice(&bytes).expect("a valid .glb");

        let names: Vec<&str> = document
            .animations()
            .filter_map(|animation| animation.name())
            .collect();
        assert_eq!(
            names,
            [IDLE],
            "the wheel's own clip is the name this plugin plays: {}",
            path.display()
        );
    }

    #[test]
    fn the_clip_match_takes_idle_and_nothing_near_it() {
        let mut app = idle_app();
        let named = model(
            &mut app,
            &[(IDLE, 4.0), ("Open", 1.0), ("Close", 1.0), ("idle", 2.0)],
        );
        assert_eq!(
            idle_clip(gltf(&app, &named)).map(|(name, _)| name),
            Some(IDLE),
            "the model's own `Idle` is the one the plugin plays"
        );

        // The near misses on their own - a door's clips, and a lower-case `idle`, which is another
        // clip a model may have rather than another spelling of this one.
        let near_misses = model(&mut app, &[("Open", 1.0), ("Close", 1.0), ("idle", 2.0)]);
        assert!(
            idle_clip(gltf(&app, &near_misses)).is_none(),
            "`idle` is not `Idle`"
        );

        // And a model with no clips at all - the overwhelming majority of the world.
        let none = model(&mut app, &[]);
        assert!(idle_clip(gltf(&app, &none)).is_none());
    }

    #[test]
    fn the_offset_is_a_phase_inside_the_clip() {
        let form_ids = [
            0x0000_0000,
            0x0000_0001,
            0x0000_0002,
            0x0001_2345,
            0x0009_2809,
            0x000F_9907,
            0x0010_0000,
            0x00FF_FFFF,
            0x7FFF_FFFF,
            0x8000_0000,
            0xFFFF_FFFE,
            0xFFFF_FFFF,
            0xDEAD_BEEF,
        ];
        for seconds in [16.666_668_f32, 8.0, 4.5, 1.0, 0.25] {
            for form_id in form_ids {
                let offset = start_offset(form_id, seconds);
                assert!(
                    offset >= 0.0 && offset < seconds,
                    "{form_id:08X} of a {seconds} s clip starts {offset} s in"
                );
                assert_eq!(
                    offset,
                    start_offset(form_id, seconds),
                    "the same reference starts in the same place every run"
                );
            }
        }

        // A clip with no length has no phase to take - and it is the case the remainder has to be
        // kept out of, since a modulo by zero is a NaN.
        assert_eq!(start_offset(0x0001_2345, 0.0), 0.0);
        assert_eq!(start_offset(0x0001_2345, -1.0), 0.0);
        assert_eq!(start_offset(0x0001_2345, f32::NAN), 0.0);
        assert_eq!(start_offset(0xFFFF_FFFF, f32::INFINITY), 0.0);
    }

    #[test]
    fn two_form_ids_of_one_model_start_at_different_times() {
        // A clip long enough to admit them: the first sixty-four FormIDs land on sixty-four
        // different seconds of a sixty-four second clip.
        let seconds = 64.0;
        let phases: HashSet<u32> = (0..64)
            .map(|form_id| (start_offset(form_id, seconds) * 1000.0) as u32)
            .collect();
        assert_eq!(phases.len(), 64, "no two of them are in lockstep");

        // And the ordinary case: two adjacent FormIDs on a wheel that turns once every 16.7 s.
        let wheel = 16.666_668;
        assert_ne!(
            start_offset(0x0001_2345, wheel),
            start_offset(0x0001_2346, wheel)
        );
    }

    #[test]
    fn every_reference_of_a_model_plays_the_clip_from_its_own_phase() {
        let mut app = idle_app();
        let wheel = model(&mut app, &[(IDLE, 8.0)]);
        let first = reference(&mut app, WATER_WHEEL, 0x0001_2345);
        let second = reference(&mut app, WATER_WHEEL, 0x0001_2347);
        let first_scene = scene(&mut app, first);
        let second_scene = scene(&mut app, second);
        pending(&mut app, first, WATER_WHEEL, wheel.clone(), None);
        pending(&mut app, second, WATER_WHEEL, wheel, None);

        step(&mut app, 1);

        let first_offset = start_offset(0x0001_2345, 8.0);
        let second_offset = start_offset(0x0001_2347, 8.0);
        assert_ne!(first_offset, second_offset, "the two are not in lockstep");
        assert_eq!(idle(&app, first).player, Some(first_scene));
        assert_eq!(idle(&app, first).offset, first_offset);
        assert_eq!(idle(&app, second).player, Some(second_scene));
        assert_eq!(idle(&app, second).offset, second_offset);
        assert!(
            app.world().get::<PendingIdleModel>(first).is_none(),
            "a reference is started once"
        );

        let node = match cache(&app).get(WATER_WHEEL) {
            Some(IdleModel::Plays(loop_)) => loop_.node,
            other => panic!("the wheel resolved to {other:?}"),
        };
        for (scene, offset) in [(first_scene, first_offset), (second_scene, second_offset)] {
            let playing = player(&app, scene)
                .animation(node)
                .expect("the clip is playing");
            assert_eq!(
                playing.repeat_mode(),
                RepeatAnimation::Forever,
                "an ambient clip loops for as long as the reference exists"
            );
            assert_eq!(
                playing.seek_time(),
                offset,
                "each copy starts at its own reference's phase"
            );
        }

        // One graph for the model, shared by both references: this is what keeps a village's forty
        // copies of a water wheel from building forty graphs.
        assert_eq!(
            app.world().resource::<Assets<AnimationGraph>>().len(),
            1,
            "one graph per model, not per reference"
        );
        let first_graph = app
            .world()
            .get::<AnimationGraphHandle>(first_scene)
            .expect("the scene's player carries the graph");
        let second_graph = app
            .world()
            .get::<AnimationGraphHandle>(second_scene)
            .expect("the scene's player carries the graph");
        assert_eq!(first_graph.0, second_graph.0);
    }

    #[test]
    fn a_model_with_no_idle_clip_is_never_looked_at_again() {
        let mut app = idle_app();
        let pile = model(&mut app, &[("LoadDust", 3.0), ("PileDust", 3.0)]);
        let first = reference(&mut app, LOG_PILE, 0x0001_0001);
        let first_scene = scene(&mut app, first);
        pending(&mut app, first, LOG_PILE, pile.clone(), None);

        step(&mut app, 1);

        assert_eq!(
            cache(&app).get(LOG_PILE),
            Some(&IdleModel::NoClip),
            "the model's answer is cached"
        );
        assert_eq!(idle(&app, first).player, None, "nothing plays on it");
        assert!(
            app.world().get::<PendingIdleModel>(first).is_none(),
            "and the reference is finished with"
        );
        assert!(
            app.world()
                .get::<AnimationGraphHandle>(first_scene)
                .is_none(),
            "no graph is built for a model with no clip"
        );

        // With the model's asset gone, a second reference of the same path can only be answered
        // from the cache: an answer that resolved again would find nothing to read and stay
        // waiting, where the cached one settles in a single frame.
        assert!(
            app.world_mut()
                .resource_mut::<Assets<Gltf>>()
                .remove(&pile)
                .is_some()
        );
        let second = reference(&mut app, LOG_PILE, 0x0001_0002);
        let second_scene = scene(&mut app, second);
        step(&mut app, 1);

        assert_eq!(idle(&app, second).player, None);
        assert!(
            app.world().get::<PendingIdleModel>(second).is_none(),
            "the cached answer settles it without asking the model again"
        );
        assert!(
            app.world()
                .get::<AnimationGraphHandle>(second_scene)
                .is_none()
        );
    }

    #[test]
    fn a_reference_whose_model_cannot_load_is_settled_and_left_still() {
        let mut app = idle_app();
        // This app registers no loader for `.glb`, so loading the model fails - which is the answer
        // this module has to give for a model the loader cannot read, since a reference left waiting
        // for one would wait for good.
        let reference = reference(&mut app, WATER_WHEEL, 0x0001_2345);

        // The answer is what this test is about, not how long the asset server takes to fail the
        // load - that is the asset server's business, it runs off the frame clock (the sleep is
        // what lets its task run under a full parallel test run), and the module answers on the
        // frame it hears. A module that never answered would leave the reference pending, and the
        // asserts below would say so.
        for _ in 0..1000 {
            app.update();
            if app.world().get::<IdleAnimation>(reference).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        assert_eq!(cache(&app).get(WATER_WHEEL), Some(&IdleModel::NoClip));
        assert_eq!(idle(&app, reference).player, None);
        assert!(app.world().get::<PendingIdleModel>(reference).is_none());
    }

    #[test]
    fn a_scene_that_never_produces_a_player_is_left_still() {
        let mut app = idle_app();
        let wheel = model(&mut app, &[(IDLE, 8.0)]);
        let reference = reference(&mut app, WATER_WHEEL, 0x0001_2345);
        // No scene under it: the model resolves, and there is no player for the clip to play on.
        pending(&mut app, reference, WATER_WHEEL, wheel, None);

        step(&mut app, 2);
        assert!(
            app.world().get::<PendingIdleModel>(reference).is_some(),
            "a scene may still be on its way"
        );

        step(&mut app, SCENE_WAIT_FRAMES);

        assert_eq!(idle(&app, reference).player, None, "nothing plays on it");
        assert!(app.world().get::<PendingIdleModel>(reference).is_none());
        assert!(
            matches!(cache(&app).get(WATER_WHEEL), Some(IdleModel::Plays(_))),
            "the model is still resolved - it is this reference that has no scene"
        );
    }

    #[test]
    fn a_load_door_is_not_claimed() {
        let mut app = idle_app();
        let door = app
            .world_mut()
            .spawn((
                MeshHandle(WATER_WHEEL.to_owned()),
                FormId(0x0009_2809),
                load_door(),
            ))
            .id();

        step(&mut app, 3);

        assert!(
            app.world().get::<IdleAnimation>(door).is_none(),
            "a door's clips are the door module's business"
        );
        assert!(app.world().get::<PendingIdleModel>(door).is_none());
        assert!(
            cache(&app).get(WATER_WHEEL).is_none(),
            "and its model was not even looked up"
        );
    }

    /// A load door, for the test that says one is left alone: the fields are irrelevant to this
    /// module, which only reads the component's presence.
    fn load_door() -> LoadDoor {
        LoadDoor {
            ref_id: 0x0009_2809,
            destination: crate::doors::DoorDestination {
                destination_ref_id: 0x0006_998D,
                interior_cell_id: Some(7),
                worldspace_id: None,
                arrival_position: [0.0; 3],
                arrival_rotation: [0.0; 3],
            },
            label: "Alftand Great Lift".into(),
            auto_load: false,
            outward: Some([1.0, 0.0, 0.0]),
        }
    }
}
