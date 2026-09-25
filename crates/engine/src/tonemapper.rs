//! Which tonemapper the main camera uses, and quick switching between them in the demo
//! (impl-217; the user's ask, 2026-09-25: compare the looks, AgX in particular).
//!
//! `--tonemapper <name>` picks one for the run - a `--shots` run included, so the same poses can
//! be rendered once per tonemapper and compared side by side - and `M` cycles the main camera to
//! the next one, naming it in the HUD's shared notices panel ([`demo_hud::Notices`]).
//!
//! Only the main camera ([`StreamingCamera`]) is touched. The portal camera keeps its own
//! `Tonemapping::None` (impl-202): it hands the doorway's image over untonemapped and the main
//! camera's tonemapper finishes it, so the doorway always follows whatever is picked here.
//!
//! Changing the tonemapper specialises a new pipeline, and Bevy skips the tonemapping pass for the
//! frame or two that takes to compile: one untonemapped (washed-out) flash on a switch is normal.

use bevy::{core_pipeline::tonemapping::Tonemapping, prelude::*};

use crate::{config::EngineConfig, demo_hud, world::components::StreamingCamera};

/// The key that cycles the main camera to the next tonemapper. `T` was taken: the pose tool
/// (`crate::pose_capture`) types a note with it.
pub const CYCLE_KEY: KeyCode = KeyCode::KeyM;

/// Every tonemapper the demo offers, in the order [`CYCLE_KEY`] walks them. The default comes
/// first and AgX, the one the user asked about, next; then KhronosPbrNeutral, the closest match to
/// vanilla Skyrim by `local/research/imagespace-colour-grading.md`.
pub const TONEMAPPERS: [(&str, Tonemapping); 8] = [
    ("TonyMcMapface", Tonemapping::TonyMcMapface),
    ("AgX", Tonemapping::AgX),
    ("KhronosPbrNeutral", Tonemapping::KhronosPbrNeutral),
    ("AcesFitted", Tonemapping::AcesFitted),
    ("BlenderFilmic", Tonemapping::BlenderFilmic),
    (
        "SomewhatBoringDisplayTransform",
        Tonemapping::SomewhatBoringDisplayTransform,
    ),
    ("Reinhard", Tonemapping::Reinhard),
    ("ReinhardLuminance", Tonemapping::ReinhardLuminance),
];

/// The tonemapper a run starts with when `--tonemapper` is not given: Bevy's own default, which
/// is what the engine has always used.
pub const DEFAULT_TONEMAPPER: Tonemapping = Tonemapping::TonyMcMapface;

/// The tonemapper `name` names, ignoring case; an unknown name is refused with the list of the
/// valid ones.
pub fn parse(name: &str) -> Result<Tonemapping, String> {
    TONEMAPPERS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name.trim()))
        .map(|(_, tonemapping)| *tonemapping)
        .ok_or_else(|| {
            let names: Vec<&str> = TONEMAPPERS.iter().map(|(known, _)| *known).collect();
            format!(
                "unknown tonemapper {name:?}; valid names (any case): {}",
                names.join(", ")
            )
        })
}

/// The name [`TONEMAPPERS`] gives `tonemapping`, or its debug name for one the demo does not offer.
pub fn name_of(tonemapping: Tonemapping) -> String {
    TONEMAPPERS
        .iter()
        .find(|(_, known)| *known == tonemapping)
        .map_or_else(
            || format!("{tonemapping:?}"),
            |(name, _)| (*name).to_owned(),
        )
}

/// The tonemapper after `current` in [`TONEMAPPERS`], wrapping round; one not in the list goes to
/// the first.
pub fn next(current: Tonemapping) -> Tonemapping {
    let index = TONEMAPPERS
        .iter()
        .position(|(_, known)| *known == current)
        .map_or(0, |index| (index + 1) % TONEMAPPERS.len());
    TONEMAPPERS[index].1
}

/// The run's `--tonemapper`, resolved before the window exists so a bad name is fatal with a
/// message rather than a silent default.
pub fn from_config(config: &EngineConfig) -> Result<Tonemapping, String> {
    config
        .portal
        .tonemapper
        .as_deref()
        .map_or(Ok(DEFAULT_TONEMAPPER), parse)
}

/// The tonemapper the main camera should use now.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainTonemapper(pub Tonemapping);

/// Applies [`MainTonemapper`] to the main camera and lets [`CYCLE_KEY`] cycle it.
pub struct TonemapperPlugin {
    /// The tonemapper the run starts with (`--tonemapper`).
    pub start: Tonemapping,
}

impl Plugin for TonemapperPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(MainTonemapper(self.start))
            .init_resource::<demo_hud::Notices>()
            .add_systems(Update, (cycle_tonemapper, apply_tonemapper).chain());
    }
}

/// `M`: the next tonemapper, named in the notices panel. No keyboard (a headless test app) is no
/// key pressed.
fn cycle_tonemapper(
    keyboard: Option<Res<ButtonInput<KeyCode>>>,
    mut tonemapper: ResMut<MainTonemapper>,
    mut notices: ResMut<demo_hud::Notices>,
) {
    if !keyboard.is_some_and(|keyboard| keyboard.just_pressed(CYCLE_KEY)) {
        return;
    }
    tonemapper.0 = next(tonemapper.0);
    notices.show(notice_text(tonemapper.0));
}

/// The notices panel's line for a switch.
fn notice_text(tonemapping: Tonemapping) -> String {
    format!("Tonemapper: {}", name_of(tonemapping))
}

/// Writes [`MainTonemapper`] to every main camera whose tonemapper differs - a camera spawned
/// later (a shots run's, a fixture's) is caught on its first frame. The portal camera is not a
/// [`StreamingCamera`] and keeps its `Tonemapping::None`.
fn apply_tonemapper(
    tonemapper: Res<MainTonemapper>,
    mut cameras: Query<&mut Tonemapping, With<StreamingCamera>>,
) {
    for mut tonemapping in &mut cameras {
        if *tonemapping != tonemapper.0 {
            *tonemapping = tonemapper.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_parses_in_any_case_and_an_unknown_one_lists_the_valid_names() {
        for (name, tonemapping) in TONEMAPPERS {
            assert_eq!(parse(name), Ok(tonemapping), "{name}");
            assert_eq!(parse(&name.to_lowercase()), Ok(tonemapping), "{name}");
            assert_eq!(parse(&name.to_uppercase()), Ok(tonemapping), "{name}");
            assert_eq!(name_of(tonemapping), name);
        }
        assert_eq!(parse("agx"), Ok(Tonemapping::AgX));
        assert_eq!(
            parse("khronospbrneutral"),
            Ok(Tonemapping::KhronosPbrNeutral)
        );
        let error = parse("filmic").expect_err("not a tonemapper");
        for (name, _) in TONEMAPPERS {
            assert!(error.contains(name), "{error} lists {name}");
        }
        assert!(parse("None").is_err(), "the portal's None is not offered");
    }

    #[test]
    fn the_flag_defaults_to_tony_mc_mapface_and_is_read_from_the_command_line() {
        let config = EngineConfig::from_args(Vec::<String>::new());
        assert_eq!(from_config(&config), Ok(Tonemapping::TonyMcMapface));
        let config = EngineConfig::from_args(["--tonemapper", "AGX"].map(str::to_owned));
        assert_eq!(from_config(&config), Ok(Tonemapping::AgX));
        let config = EngineConfig::from_args(["--tonemapper", "nope"].map(str::to_owned));
        assert!(from_config(&config).is_err());
    }

    #[test]
    fn cycling_walks_every_tonemapper_once_from_the_default_and_wraps() {
        let mut current = DEFAULT_TONEMAPPER;
        let mut seen = vec![current];
        for _ in 1..TONEMAPPERS.len() {
            current = next(current);
            seen.push(current);
        }
        let order: Vec<Tonemapping> = TONEMAPPERS.iter().map(|(_, t)| *t).collect();
        assert_eq!(seen, order);
        assert_eq!(
            next(DEFAULT_TONEMAPPER),
            Tonemapping::AgX,
            "AgX is one press away"
        );
        assert_eq!(next(current), DEFAULT_TONEMAPPER, "wraps round");
        assert_eq!(next(Tonemapping::None), TONEMAPPERS[0].1);
    }

    #[test]
    fn the_key_cycles_the_main_camera_only_and_names_the_tonemapper() {
        let mut app = App::new();
        app.init_resource::<ButtonInput<KeyCode>>()
            .add_plugins(TonemapperPlugin {
                start: Tonemapping::KhronosPbrNeutral,
            });
        let main = app
            .world_mut()
            .spawn((StreamingCamera, Tonemapping::TonyMcMapface))
            .id();
        // The portal camera: not a streaming camera, and handed over untonemapped.
        let portal = app.world_mut().spawn(Tonemapping::None).id();

        app.update();
        assert_eq!(
            app.world().get::<Tonemapping>(main),
            Some(&Tonemapping::KhronosPbrNeutral),
            "the flag's tonemapper is applied from the first frame"
        );

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(CYCLE_KEY);
        app.update();
        assert_eq!(
            app.world().get::<Tonemapping>(main),
            Some(&Tonemapping::AcesFitted)
        );
        assert_eq!(
            app.world().resource::<demo_hud::Notices>().text(),
            Some("Tonemapper: AcesFitted")
        );
        assert_eq!(
            app.world().get::<Tonemapping>(portal),
            Some(&Tonemapping::None),
            "the portal camera keeps its own"
        );

        // Held, not pressed again: no further change.
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        app.update();
        assert_eq!(
            app.world().get::<Tonemapping>(main),
            Some(&Tonemapping::AcesFitted)
        );
    }
}
