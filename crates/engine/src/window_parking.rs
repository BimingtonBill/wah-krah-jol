//! The window of an automated run - a scripted tour, a shots run - kept out of the user's way but
//! within reach (the user asked for both, 2026-09-24).
//!
//! It starts **parked**: at a position no monitor shows, not focused, but with its taskbar entry.
//! A parked window is still a full-size window, so it renders and its screenshots are exactly as
//! on screen. Clicking its taskbar entry focuses it, and a focused window is brought to the middle
//! of the screen to be watched; when it loses focus it is parked again. Minimising parks it too,
//! and restores it: a truly minimised window has a zero-size surface, which Bevy renders at 1x1
//! pixels - every screenshot of a tour run minimised came out one black pixel (measured the same
//! day), while its door checks still passed.

use bevy::{
    prelude::*,
    window::{PrimaryWindow, WindowFocused, WindowResized},
};

/// Where a parked window stands: far outside every monitor, where Windows itself keeps minimised
/// windows, but as a normal, full-size window.
pub const PARKED_POSITION: IVec2 = IVec2::new(-32000, -32000);

/// Parks an automated run's window and brings it on screen while it has focus. Added by
/// [`crate::portal::PortalPlugin`] for the runs that open their window parked
/// ([`crate::config::EngineConfig::window_offscreen`]).
pub struct WindowParkingPlugin;

impl Plugin for WindowParkingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, park_and_unpark);
    }
}

/// Where the window should be for what happened to it this frame, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowPlace {
    /// Focused: in the middle of the screen, to be watched.
    OnScreen,
    /// Unfocused, or minimised: parked, still rendering.
    Parked,
}

/// What the window's events of one frame ask for: the last focus change wins, and a minimise (a
/// zero-size resize) always parks.
fn wanted_place(focus: impl IntoIterator<Item = bool>, minimised: bool) -> Option<WindowPlace> {
    if minimised {
        return Some(WindowPlace::Parked);
    }
    focus.into_iter().last().map(|focused| {
        if focused {
            WindowPlace::OnScreen
        } else {
            WindowPlace::Parked
        }
    })
}

fn park_and_unpark(
    mut focus: MessageReader<WindowFocused>,
    mut resized: MessageReader<WindowResized>,
    mut windows: Query<(Entity, &mut Window), With<PrimaryWindow>>,
) {
    let Ok((entity, mut window)) = windows.single_mut() else {
        focus.clear();
        resized.clear();
        return;
    };
    let minimised = resized
        .read()
        .any(|event| event.window == entity && (event.width <= 0.0 || event.height <= 0.0));
    let focus_changes: Vec<bool> = focus
        .read()
        .filter(|event| event.window == entity)
        .map(|event| event.focused)
        .collect();
    match wanted_place(focus_changes, minimised) {
        Some(WindowPlace::OnScreen) => {
            window.position = WindowPosition::Centered(MonitorSelection::Current);
        }
        Some(WindowPlace::Parked) => {
            if minimised {
                window.set_minimized(false);
            }
            window.position = WindowPosition::At(PARKED_POSITION);
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_brings_the_window_on_screen_and_losing_it_or_minimising_parks_it() {
        assert_eq!(wanted_place([true], false), Some(WindowPlace::OnScreen));
        assert_eq!(wanted_place([false], false), Some(WindowPlace::Parked));
        assert_eq!(
            wanted_place([false, true], false),
            Some(WindowPlace::OnScreen),
            "the last focus change of the frame wins"
        );
        assert_eq!(
            wanted_place([true], true),
            Some(WindowPlace::Parked),
            "a minimise parks it, whatever the focus did"
        );
        assert_eq!(
            wanted_place([], false),
            None,
            "nothing happened: nothing moves"
        );
    }
}
