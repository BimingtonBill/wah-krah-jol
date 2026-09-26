//! Which tonemapper the main camera uses, and quick switching between them in the demo
//! (impl-217; the user's ask, 2026-09-25: compare the looks, AgX in particular).
//!
//! `--tonemapper <name>` picks one for the run - a `--shots` run included, so the same poses can
//! be rendered once per tonemapper and compared side by side - and `M` cycles the main camera to
//! the next one, naming it in the HUD's shared notices panel ([`demo_hud::Notices`]).
//!
//! The tonemapper is one of the graphics settings ([`GraphicsSettings::tonemapper`], impl-219),
//! applied to the main camera by `crate::graphics_settings`. The portal camera keeps its own
//! `Tonemapping::None` (impl-202): it hands the doorway's image over untonemapped and the main
//! camera's tonemapper finishes it, so the doorway always follows whatever is picked here.
//!
//! Changing the tonemapper specialises a new pipeline, and Bevy skips the tonemapping pass for the
//! frame or two that takes to compile: one untonemapped (washed-out) flash on a switch is normal.

use bevy::{core_pipeline::tonemapping::Tonemapping, prelude::*};

use crate::{demo_hud, graphics_settings::GraphicsSettings};

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

/// `M`: the next tonemapper, named in the notices panel; the settings then read as `custom`
/// unless that lands on a preset's own. No keyboard (a headless test app) is no key pressed.
/// Registered by `crate::graphics_settings::GraphicsSettingsPlugin`, which applies the change.
pub(crate) fn cycle_tonemapper(
    keyboard: Option<Res<ButtonInput<KeyCode>>>,
    mut settings: ResMut<GraphicsSettings>,
    mut notices: ResMut<demo_hud::Notices>,
) {
    if !keyboard.is_some_and(|keyboard| keyboard.just_pressed(CYCLE_KEY)) {
        return;
    }
    settings.tonemapper = next(settings.tonemapper);
    settings.relabel();
    notices.show(notice_text(settings.tonemapper));
}

/// The notices panel's line for a switch.
fn notice_text(tonemapping: Tonemapping) -> String {
    format!("Tonemapper: {}", name_of(tonemapping))
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
}
