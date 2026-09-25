//! The in-game graphics panel: every [`GraphicsSettings`] knob on screen, changed live
//! (impl-220; plan `local/research/fun/rendering-plan.md` step 1, the third way in after the flags
//! and the settings file of impl-219).
//!
//! `G` opens and closes it (`Esc` closes it too). While it is open:
//!
//! * `Up`/`Down`, or `Tab`/`Shift+Tab`, move between the knobs;
//! * `Left`/`Right`, or `-`/`+` (`=` and the keypad's too), change the selected one: a choice
//!   (anti-aliasing, SSAO, the tonemapper, ...) steps through its values and wraps round, a number
//!   steps by a fixed amount inside the range the flag accepts;
//! * `1`/`2`/`3` switch to the `current`, `bevy` and `custom` presets. `custom` brings back the
//!   last settings that read as custom in this run (the file's, the flags', or the panel's own
//!   edits), or `current` relabelled when there were none;
//! * `S` saves the settings to [`graphics_settings::DEFAULT_FILE`] (`local/graphics.toml`), which
//!   the next run reads ([`graphics_settings::from_config`]) and which parses back to exactly the
//!   same settings ([`settings_file_text`]).
//!
//! Every change is written to the [`GraphicsSettings`] resource, which
//! `crate::graphics_settings` applies to the cameras and lights the same frame. A change that
//! specialises new pipelines (anti-aliasing, SSAO, the tonemapper, the shadow filter, contact
//! shadows) is drawn without them for a frame or two while they compile: the panel's footer says
//! so.
//!
//! While the panel is open every other key, the mouse buttons and mouse look are blocked the way
//! `crate::field_notes` blocks them while its note box is open ([`panel_input`], in `PreUpdate`
//! after Bevy's own input systems, reads the panel's keys and then resets the button state before
//! anything in `Update` reads it) - so `S` saves rather than walks backwards and `M` does not also
//! cycle the tonemapper.
//!
//! An automated run hides the panel with the rest of the HUD ([`demo_hud::hidden_for_this_run`]).
//! For a screenshot of it, `--show-window` plus the environment variable [`OPEN_AT_START_ENV`]
//! starts the run with the panel open.

use std::path::Path;

use bevy::{
    core_pipeline::tonemapping::Tonemapping,
    input::{InputSystems, mouse::AccumulatedMouseMotion},
    prelude::*,
};

use crate::{
    config::EngineConfig,
    demo_hud,
    graphics_settings::{
        self, AntiAliasing, ExposureMode, GraphicsSettings, MAX_SHADOW_CASCADES, Preset,
        ShadowFilter, Ssao,
    },
    tonemapper,
};

/// `G`: open or close the panel.
pub const TOGGLE_KEY: KeyCode = KeyCode::KeyG;

/// Set to `1` (or anything but `0`) to start a run with the panel open: the hidden switch for an
/// automated screenshot of it (with `--show-window`, or the HUD stays hidden).
pub const OPEN_AT_START_ENV: &str = "OPENSKYRIM_GRAPHICS_PANEL";

/// The footer's warning about the frame a pipeline change takes to compile.
pub const FLASH_NOTE: &str =
    "A change that needs new pipelines (AA, SSAO, tonemapper, shadows) may flash for a frame.";

/// Every knob the panel shows, in the order it lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    Aa,
    Ssao,
    SsaoRadius,
    SsaoThickness,
    Exposure,
    Ev100,
    ExposureMin,
    ExposureMax,
    ExposureSpeed,
    ExposureSpeedDown,
    Tonemapper,
    Bloom,
    BloomIntensity,
    ShadowMapSize,
    ShadowCascades,
    ShadowDistance,
    ShadowFilter,
    ContactShadows,
    PortalScale,
}

impl Knob {
    /// Every knob, in the panel's order: the flags' own ([`graphics_settings::KNOBS`]) with the
    /// fixed EV100 and the tonemapper added where they belong.
    pub const ALL: [Knob; 19] = [
        Knob::Aa,
        Knob::Ssao,
        Knob::SsaoRadius,
        Knob::SsaoThickness,
        Knob::Exposure,
        Knob::Ev100,
        Knob::ExposureMin,
        Knob::ExposureMax,
        Knob::ExposureSpeed,
        Knob::ExposureSpeedDown,
        Knob::Tonemapper,
        Knob::Bloom,
        Knob::BloomIntensity,
        Knob::ShadowMapSize,
        Knob::ShadowCascades,
        Knob::ShadowDistance,
        Knob::ShadowFilter,
        Knob::ContactShadows,
        Knob::PortalScale,
    ];

    /// The knob's label: its flag's name, so the panel, the flags and the file read alike.
    pub fn label(self) -> &'static str {
        match self {
            Knob::Aa => "aa",
            Knob::Ssao => "ssao",
            Knob::SsaoRadius => "ssao-radius",
            Knob::SsaoThickness => "ssao-thickness",
            Knob::Exposure => "exposure",
            Knob::Ev100 => "ev100",
            Knob::ExposureMin => "exposure-min",
            Knob::ExposureMax => "exposure-max",
            Knob::ExposureSpeed => "exposure-speed",
            Knob::ExposureSpeedDown => "exposure-speed-down",
            Knob::Tonemapper => "tonemapper",
            Knob::Bloom => "bloom",
            Knob::BloomIntensity => "bloom-intensity",
            Knob::ShadowMapSize => "shadow-map-size",
            Knob::ShadowCascades => "shadow-cascades",
            Knob::ShadowDistance => "shadow-distance",
            Knob::ShadowFilter => "shadow-filter",
            Knob::ContactShadows => "contact-shadows",
            Knob::PortalScale => "portal-scale",
        }
    }

    /// The knob's current value, as the flag would spell it.
    pub fn value(self, settings: &GraphicsSettings) -> String {
        match self {
            Knob::Aa => aa_name(settings.aa).to_owned(),
            Knob::Ssao => ssao_name(settings.ssao).to_owned(),
            Knob::SsaoRadius => settings.ssao_radius.to_string(),
            Knob::SsaoThickness => settings.ssao_thickness.to_string(),
            Knob::Exposure => exposure_name(settings.exposure).to_owned(),
            Knob::Ev100 => settings.ev100.to_string(),
            Knob::ExposureMin => settings.exposure_min.to_string(),
            Knob::ExposureMax => settings.exposure_max.to_string(),
            Knob::ExposureSpeed => settings.exposure_speed.to_string(),
            Knob::ExposureSpeedDown => settings.exposure_speed_down.to_string(),
            Knob::Tonemapper => tonemapper::name_of(settings.tonemapper),
            Knob::Bloom => on_off(settings.bloom).to_owned(),
            Knob::BloomIntensity => settings.bloom_intensity.to_string(),
            Knob::ShadowMapSize => settings.shadow_map_size.to_string(),
            Knob::ShadowCascades => settings.shadow_cascades.to_string(),
            Knob::ShadowDistance => settings
                .shadow_distance
                .map_or_else(|| "stream".to_owned(), |distance| distance.to_string()),
            Knob::ShadowFilter => shadow_filter_name(settings.shadow_filter).to_owned(),
            Knob::ContactShadows => on_off(settings.contact_shadows).to_owned(),
            Knob::PortalScale => settings.portal_scale.to_string(),
        }
    }

    /// A note shown after the value: what the knob only does in some modes, or does not do yet.
    fn note(self, settings: &GraphicsSettings) -> &'static str {
        match self {
            Knob::SsaoRadius | Knob::SsaoThickness if settings.ssao == Ssao::Off => "  (ssao off)",
            Knob::ExposureMin
            | Knob::ExposureMax
            | Knob::ExposureSpeed
            | Knob::ExposureSpeedDown
                if settings.exposure == ExposureMode::Fixed =>
            {
                "  (auto only)"
            }
            Knob::BloomIntensity if !settings.bloom => "  (bloom off)",
            Knob::PortalScale => "  (not applied yet)",
            _ => "",
        }
    }

    /// Steps the knob once in `direction` (+1 or -1): a choice to its next or previous value,
    /// wrapping round; a number by its step, inside the range its flag accepts.
    pub fn step(self, settings: &mut GraphicsSettings, direction: i32) {
        let up = direction > 0;
        match self {
            Knob::Aa => {
                settings.aa = cycle(
                    &[
                        AntiAliasing::Off,
                        AntiAliasing::Fxaa,
                        AntiAliasing::Smaa,
                        AntiAliasing::Taa,
                    ],
                    settings.aa,
                    up,
                );
            }
            Knob::Ssao => {
                settings.ssao = cycle(
                    &[Ssao::Off, Ssao::Low, Ssao::Medium, Ssao::High, Ssao::Ultra],
                    settings.ssao,
                    up,
                );
            }
            Knob::SsaoRadius => {
                settings.ssao_radius = step_number(settings.ssao_radius, 5.0, 5.0, 500.0, up);
            }
            Knob::SsaoThickness => {
                settings.ssao_thickness = step_number(settings.ssao_thickness, 2.5, 2.5, 200.0, up);
            }
            Knob::Exposure => {
                settings.exposure = cycle(
                    &[ExposureMode::Fixed, ExposureMode::Auto],
                    settings.exposure,
                    up,
                );
            }
            Knob::Ev100 => settings.ev100 = step_number(settings.ev100, 0.5, -4.0, 20.0, up),
            Knob::ExposureMin => {
                settings.exposure_min = step_number(
                    settings.exposure_min,
                    0.5,
                    -16.0,
                    settings.exposure_max - 0.5,
                    up,
                );
            }
            Knob::ExposureMax => {
                settings.exposure_max = step_number(
                    settings.exposure_max,
                    0.5,
                    settings.exposure_min + 0.5,
                    16.0,
                    up,
                );
            }
            Knob::ExposureSpeed => {
                settings.exposure_speed = step_number(settings.exposure_speed, 0.5, 0.5, 20.0, up);
            }
            Knob::ExposureSpeedDown => {
                settings.exposure_speed_down =
                    step_number(settings.exposure_speed_down, 0.5, 0.5, 20.0, up);
            }
            Knob::Tonemapper => {
                settings.tonemapper = if up {
                    tonemapper::next(settings.tonemapper)
                } else {
                    previous_tonemapper(settings.tonemapper)
                };
            }
            Knob::Bloom => settings.bloom = !settings.bloom,
            Knob::BloomIntensity => {
                settings.bloom_intensity =
                    step_number(settings.bloom_intensity, 0.05, 0.0, 1.0, up);
            }
            Knob::ShadowMapSize => {
                let size = settings.shadow_map_size;
                settings.shadow_map_size = if up {
                    (size * 2).min(8192)
                } else {
                    (size / 2).max(256)
                };
            }
            Knob::ShadowCascades => {
                let count = settings.shadow_cascades;
                settings.shadow_cascades = if up {
                    (count + 1).min(MAX_SHADOW_CASCADES)
                } else {
                    count.saturating_sub(1).max(1)
                };
            }
            Knob::ShadowDistance => {
                settings.shadow_distance = step_shadow_distance(settings.shadow_distance, up);
            }
            Knob::ShadowFilter => {
                settings.shadow_filter = cycle(
                    &[
                        ShadowFilter::Gaussian,
                        ShadowFilter::Hardware2x2,
                        ShadowFilter::Temporal,
                    ],
                    settings.shadow_filter,
                    up,
                );
            }
            Knob::ContactShadows => settings.contact_shadows = !settings.contact_shadows,
            Knob::PortalScale => {
                settings.portal_scale = step_number(settings.portal_scale, 0.05, 0.1, 1.0, up);
            }
        }
    }
}

/// The shadow reaches `shadow-distance` steps through, after `stream` (the reach fitted to the
/// stream radius), in Creation units.
pub const SHADOW_DISTANCES: [f32; 7] = [1000.0, 2000.0, 4000.0, 8000.0, 16000.0, 32000.0, 64000.0];

fn step_shadow_distance(current: Option<f32>, up: bool) -> Option<f32> {
    match (current, up) {
        (None, true) => Some(SHADOW_DISTANCES[0]),
        (None, false) => None,
        (Some(distance), true) => Some(
            SHADOW_DISTANCES
                .iter()
                .copied()
                .find(|step| *step > distance)
                .unwrap_or(distance.max(SHADOW_DISTANCES[SHADOW_DISTANCES.len() - 1])),
        ),
        (Some(distance), false) => SHADOW_DISTANCES
            .iter()
            .rev()
            .copied()
            .find(|step| *step < distance),
    }
}

fn cycle<T: Copy + PartialEq>(values: &[T], current: T, up: bool) -> T {
    let index = values
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0);
    let next = if up {
        (index + 1) % values.len()
    } else {
        (index + values.len() - 1) % values.len()
    };
    values[next]
}

/// `value` one `step` up or down, clamped to `min..=max` and rounded to a thousandth so repeated
/// steps do not drift (0.15 + 0.05 is 0.2, not 0.20000002).
fn step_number(value: f32, step: f32, min: f32, max: f32, up: bool) -> f32 {
    let moved = if up { value + step } else { value - step };
    ((moved.clamp(min, max) * 1000.0).round() / 1000.0).clamp(min, max)
}

fn previous_tonemapper(current: Tonemapping) -> Tonemapping {
    let list = &tonemapper::TONEMAPPERS;
    let index = list
        .iter()
        .position(|(_, known)| *known == current)
        .map_or(0, |index| (index + list.len() - 1) % list.len());
    list[index].1
}

fn aa_name(aa: AntiAliasing) -> &'static str {
    match aa {
        AntiAliasing::Off => "off",
        AntiAliasing::Fxaa => "fxaa",
        AntiAliasing::Smaa => "smaa",
        AntiAliasing::Taa => "taa",
    }
}

fn ssao_name(ssao: Ssao) -> &'static str {
    match ssao {
        Ssao::Off => "off",
        Ssao::Low => "low",
        Ssao::Medium => "medium",
        Ssao::High => "high",
        Ssao::Ultra => "ultra",
    }
}

fn exposure_name(exposure: ExposureMode) -> &'static str {
    match exposure {
        ExposureMode::Fixed => "fixed",
        ExposureMode::Auto => "auto",
    }
}

fn shadow_filter_name(filter: ShadowFilter) -> &'static str {
    match filter {
        ShadowFilter::Gaussian => "gaussian",
        ShadowFilter::Hardware2x2 => "hardware2x2",
        ShadowFilter::Temporal => "temporal",
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

// ---------------------------------------------------------------------------------------------
// The panel's state
// ---------------------------------------------------------------------------------------------

/// Whether the panel is open, which knob is selected, and the custom settings `3` brings back.
#[derive(Resource, Debug, Clone, Default)]
pub struct GraphicsPanel {
    pub open: bool,
    /// An index into [`Knob::ALL`].
    pub selected: usize,
    /// The last settings in this run that read as `custom`.
    pub last_custom: Option<GraphicsSettings>,
}

impl GraphicsPanel {
    /// The knob the cursor is on.
    pub fn selected_knob(&self) -> Knob {
        Knob::ALL[self.selected % Knob::ALL.len()]
    }

    /// Moves the cursor `direction` knobs down (+1) or up (-1), wrapping round.
    pub fn move_selection(&mut self, direction: i32) {
        let count = Knob::ALL.len() as i32;
        self.selected = (self.selected as i32 + direction).rem_euclid(count) as usize;
    }

    /// Steps the selected knob, labels the settings with the preset they now equal (`custom` if
    /// none), and remembers them if they read as `custom`.
    pub fn change(&mut self, settings: &mut GraphicsSettings, direction: i32) {
        self.selected_knob().step(settings, direction);
        // Relabel from scratch rather than from the preset the edit started on (which
        // `GraphicsSettings::relabel` would keep once it is `custom`): stepping back onto a
        // preset's own values reads as that preset again.
        settings.preset = Preset::Current;
        settings.relabel();
        self.remember(settings);
    }

    /// Keeps `settings` as the ones `3` brings back if they read as `custom`.
    pub fn remember(&mut self, settings: &GraphicsSettings) {
        if settings.preset == Preset::Custom {
            self.last_custom = Some(settings.clone());
        }
    }

    /// `1`/`2`/`3`: the named preset's settings; `custom` is the last custom settings of the run,
    /// or `current` relabelled.
    pub fn preset_settings(&self, preset: Preset) -> GraphicsSettings {
        match preset {
            Preset::Custom => self
                .last_custom
                .clone()
                .unwrap_or_else(|| Preset::Custom.settings()),
            named => named.settings(),
        }
    }
}

/// The panel's text: title and preset, one line per knob with the cursor on the selected one, and
/// the keys.
pub fn panel_text(panel: &GraphicsPanel, settings: &GraphicsSettings) -> String {
    let width = Knob::ALL
        .iter()
        .map(|knob| knob.label().len())
        .max()
        .unwrap_or(0);
    let mut lines = vec![
        "Graphics settings".to_owned(),
        format!(
            "Preset: {}    (1: current  2: bevy  3: custom)",
            settings.preset.name()
        ),
        String::new(),
    ];
    for (index, knob) in Knob::ALL.iter().enumerate() {
        let cursor = if index == panel.selected % Knob::ALL.len() {
            ">"
        } else {
            " "
        };
        lines.push(format!(
            "{cursor} {:<width$}  {}{}",
            knob.label(),
            knob.value(settings),
            knob.note(settings)
        ));
    }
    lines.push(String::new());
    lines.push("Up/Down or Tab: knob  |  Left/Right or -/+: change".to_owned());
    lines.push(format!(
        "S: save to {}  |  G or Esc: close",
        graphics_settings::DEFAULT_FILE
    ));
    lines.push(FLASH_NOTE.to_owned());
    lines.join("\n")
}

// ---------------------------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------------------------

/// The settings as the flat TOML file [`graphics_settings::parse_file`] reads, every knob written
/// out, so reading it back gives exactly these settings.
///
/// The fixed EV100 has no key of its own: `exposure = <number>` sets it (and fixed mode), so an
/// auto-exposure file writes that line and then `exposure = "auto"`; the reader applies the lines
/// in order. The metering range's two ends are written in the order that never has the
/// minimum above the maximum between them.
pub fn settings_file_text(settings: &GraphicsSettings) -> String {
    let mut lines = vec![
        "# Graphics settings, saved from the in-game panel (G). Read at start unless".to_owned(),
        "# --graphics-file names another file; flags on the command line override it.".to_owned(),
        format!("graphics = \"{}\"", settings.preset.name()),
        format!("aa = \"{}\"", aa_name(settings.aa)),
        format!("ssao = \"{}\"", ssao_name(settings.ssao)),
        format!("ssao_radius = {}", settings.ssao_radius),
        format!("ssao_thickness = {}", settings.ssao_thickness),
        "# The fixed EV100; auto exposure corrects on top of it.".to_owned(),
        format!("exposure = {}", settings.ev100),
    ];
    if settings.exposure == ExposureMode::Auto {
        lines.push("exposure = \"auto\"".to_owned());
    }
    let min = format!("exposure_min = {}", settings.exposure_min);
    let max = format!("exposure_max = {}", settings.exposure_max);
    if settings.exposure_min < GraphicsSettings::current().exposure_max {
        lines.extend([min, max]);
    } else {
        lines.extend([max, min]);
    }
    lines.extend([
        // `exposure_speed` sets both directions; the downward one follows it.
        format!("exposure_speed = {}", settings.exposure_speed),
        format!("exposure_speed_down = {}", settings.exposure_speed_down),
        format!(
            "tonemapper = \"{}\"",
            tonemapper::name_of(settings.tonemapper)
        ),
        format!("bloom = \"{}\"", on_off(settings.bloom)),
        format!("bloom_intensity = {}", settings.bloom_intensity),
        format!("shadow_map_size = {}", settings.shadow_map_size),
        format!("shadow_cascades = {}", settings.shadow_cascades),
        format!(
            "shadow_distance = \"{}\"",
            Knob::ShadowDistance.value(settings)
        ),
        format!(
            "shadow_filter = \"{}\"",
            shadow_filter_name(settings.shadow_filter)
        ),
        format!("contact_shadows = \"{}\"", on_off(settings.contact_shadows)),
        format!("portal_scale = {}", settings.portal_scale),
    ]);
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Writes [`settings_file_text`] to `path`, creating its folder.
pub fn save(path: &Path, settings: &GraphicsSettings) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, settings_file_text(settings))
}

/// The settings a saved file gives a run with no flags: [`graphics_settings::resolve`] on it.
pub fn load(path: &Path) -> Result<GraphicsSettings, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let pairs = graphics_settings::parse_file(path, &text)?;
    graphics_settings::resolve(None, Some(&pairs), &[], None)
}

// ---------------------------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------------------------

/// Adds the panel to a windowed run that has opened the world (`crate::app::run`, beside the field
/// notes). Needs [`GraphicsSettings`] (`crate::graphics_settings::GraphicsSettingsPlugin`).
pub struct GraphicsPanelPlugin;

impl Plugin for GraphicsPanelPlugin {
    fn build(&self, app: &mut App) {
        let open = std::env::var(OPEN_AT_START_ENV).is_ok_and(|value| value.trim() != "0");
        let mut panel = GraphicsPanel { open, ..default() };
        if let Some(settings) = app.world().get_resource::<GraphicsSettings>() {
            panel.remember(settings);
        }
        app.insert_resource(panel)
            .init_resource::<demo_hud::Notices>()
            .add_systems(Startup, spawn_panel)
            .add_systems(
                PreUpdate,
                panel_input
                    .after(InputSystems)
                    .after(crate::field_notes::block_input_while_typing),
            )
            .add_systems(Update, sync_panel);
    }
}

/// The panel's own entity.
#[derive(Component)]
pub struct GraphicsPanelText;

fn spawn_panel(mut commands: Commands) {
    let (mut node, background) = demo_hud::panel_node(Display::None);
    node.top = demo_hud::MARGIN;
    node.right = demo_hud::MARGIN;
    commands.spawn((
        GraphicsPanelText,
        node,
        background,
        demo_hud::text(String::new(), demo_hud::FONT_SIZE),
    ));
}

/// Reads the panel's keys, changes the settings, and while the panel is open blocks every key,
/// mouse button and mouse look from the rest of the frame. No keyboard (a headless test app) is
/// no key pressed.
pub fn panel_input(
    keyboard: Option<ResMut<ButtonInput<KeyCode>>>,
    mouse_buttons: Option<ResMut<ButtonInput<MouseButton>>>,
    mouse_motion: Option<ResMut<AccumulatedMouseMotion>>,
    mut panel: ResMut<GraphicsPanel>,
    mut settings: ResMut<GraphicsSettings>,
    mut notices: ResMut<demo_hud::Notices>,
) {
    let Some(mut keyboard) = keyboard else {
        return;
    };
    let was_open = panel.open;
    if keyboard.just_pressed(TOGGLE_KEY) || (panel.open && keyboard.just_pressed(KeyCode::Escape)) {
        panel.open = !panel.open;
    } else if panel.open {
        handle_panel_keys(&keyboard, &mut panel, &mut settings, &mut notices);
    }
    if panel.open || was_open {
        keyboard.reset_all();
        if let Some(mut buttons) = mouse_buttons {
            buttons.reset_all();
        }
        if let Some(mut motion) = mouse_motion {
            motion.delta = Vec2::ZERO;
        }
    }
}

fn handle_panel_keys(
    keyboard: &ButtonInput<KeyCode>,
    panel: &mut GraphicsPanel,
    settings: &mut ResMut<GraphicsSettings>,
    notices: &mut demo_hud::Notices,
) {
    let pressed = |keys: &[KeyCode]| keys.iter().any(|key| keyboard.just_pressed(*key));
    let shift = keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight);
    if pressed(&[KeyCode::ArrowDown]) || (pressed(&[KeyCode::Tab]) && !shift) {
        panel.move_selection(1);
    }
    if pressed(&[KeyCode::ArrowUp]) || (pressed(&[KeyCode::Tab]) && shift) {
        panel.move_selection(-1);
    }
    let mut changed = settings.clone();
    if pressed(&[KeyCode::ArrowRight, KeyCode::Equal, KeyCode::NumpadAdd]) {
        panel.change(&mut changed, 1);
    }
    if pressed(&[KeyCode::ArrowLeft, KeyCode::Minus, KeyCode::NumpadSubtract]) {
        panel.change(&mut changed, -1);
    }
    for (keys, preset) in [
        ([KeyCode::Digit1, KeyCode::Numpad1], Preset::Current),
        ([KeyCode::Digit2, KeyCode::Numpad2], Preset::Bevy),
        ([KeyCode::Digit3, KeyCode::Numpad3], Preset::Custom),
    ] {
        if pressed(&keys) {
            changed = panel.preset_settings(preset);
            notices.show(format!("Graphics preset: {}", preset.name()));
        }
    }
    // Only a real change touches the resource, so its change detection re-applies the cameras
    // only when something moved.
    if changed != **settings {
        **settings = changed;
    }
    if pressed(&[KeyCode::KeyS]) {
        let path = Path::new(graphics_settings::DEFAULT_FILE);
        match save(path, settings) {
            Ok(()) => {
                info!(
                    "graphics panel saved {}: {}",
                    path.display(),
                    settings.summary()
                );
                notices.show(format!("Saved graphics settings to {}", path.display()));
            }
            Err(error) => {
                warn!("graphics panel could not save {}: {error}", path.display());
                notices.show(format!("Could not save {}: {error}", path.display()));
            }
        }
    }
}

/// Shows or hides the panel and fills it, suppressed like the rest of the HUD in a run whose
/// screenshots must stay clean.
fn sync_panel(
    config: Res<EngineConfig>,
    panel: Res<GraphicsPanel>,
    settings: Res<GraphicsSettings>,
    mut texts: Query<(&mut Text, &mut Node), With<GraphicsPanelText>>,
) {
    let Ok((mut text, mut node)) = texts.single_mut() else {
        return;
    };
    if panel.open && !demo_hud::hidden_for_this_run(&config) {
        let value = panel_text(&panel, &settings);
        if text.as_str() != value {
            **text = value;
        }
        node.display = Display::Flex;
    } else {
        node.display = Display::None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    fn index_of(knob: Knob) -> usize {
        Knob::ALL.iter().position(|known| *known == knob).unwrap()
    }

    fn panel_at(knob: Knob) -> GraphicsPanel {
        GraphicsPanel {
            selected: index_of(knob),
            ..default()
        }
    }

    #[test]
    fn navigation_walks_every_knob_and_wraps_both_ways() {
        let mut panel = GraphicsPanel::default();
        assert_eq!(panel.selected_knob(), Knob::Aa);
        let mut seen = vec![panel.selected_knob()];
        for _ in 1..Knob::ALL.len() {
            panel.move_selection(1);
            seen.push(panel.selected_knob());
        }
        assert_eq!(seen, Knob::ALL.to_vec(), "down visits every knob in order");
        panel.move_selection(1);
        assert_eq!(panel.selected_knob(), Knob::Aa, "wraps past the last");
        panel.move_selection(-1);
        assert_eq!(
            panel.selected_knob(),
            Knob::PortalScale,
            "wraps past the first"
        );
    }

    #[test]
    fn every_flag_knob_is_on_the_panel() {
        let labels: Vec<&str> = Knob::ALL.iter().map(|knob| knob.label()).collect();
        for knob in graphics_settings::KNOBS {
            assert!(labels.contains(&knob), "{knob} is missing from the panel");
        }
        assert!(labels.contains(&"tonemapper") && labels.contains(&"ev100"));
    }

    #[test]
    fn every_knob_changes_and_stays_within_its_flags_bounds() {
        // Each knob stepped far past both ends must stay a value its own flag accepts: the
        // settings file written from it is read back by `GraphicsSettings::set`.
        for knob in Knob::ALL {
            for direction in [1, -1] {
                let mut settings = GraphicsSettings::current();
                let mut panel = panel_at(knob);
                for _ in 0..400 {
                    panel.change(&mut settings, direction);
                    let text = settings_file_text(&settings);
                    let pairs =
                        graphics_settings::parse_file(Path::new("panel.toml"), &text).unwrap();
                    let read = graphics_settings::resolve(None, Some(&pairs), &[], None)
                        .unwrap_or_else(|error| panic!("{knob:?} {direction}: {error}"));
                    assert_eq!(read, settings, "{knob:?} {direction}");
                }
                // Every knob moves at least one way from `current`.
                if direction == 1 {
                    let mut up = GraphicsSettings::current();
                    let mut down = GraphicsSettings::current();
                    knob.step(&mut up, 1);
                    knob.step(&mut down, -1);
                    assert!(
                        up != GraphicsSettings::current() || down != GraphicsSettings::current(),
                        "{knob:?} never changes"
                    );
                }
            }
        }
    }

    #[test]
    fn numbers_clamp_at_their_ends_and_choices_wrap() {
        let mut settings = GraphicsSettings::current();
        for _ in 0..50 {
            Knob::BloomIntensity.step(&mut settings, 1);
        }
        assert_eq!(settings.bloom_intensity, 1.0);
        for _ in 0..50 {
            Knob::BloomIntensity.step(&mut settings, -1);
        }
        assert_eq!(settings.bloom_intensity, 0.0);
        Knob::BloomIntensity.step(&mut settings, 1);
        Knob::BloomIntensity.step(&mut settings, 1);
        Knob::BloomIntensity.step(&mut settings, 1);
        assert_eq!(
            settings.bloom_intensity, 0.15,
            "no drift from repeated steps"
        );

        for _ in 0..10 {
            Knob::ShadowMapSize.step(&mut settings, 1);
        }
        assert_eq!(settings.shadow_map_size, 8192);
        for _ in 0..10 {
            Knob::ShadowMapSize.step(&mut settings, -1);
        }
        assert_eq!(settings.shadow_map_size, 256);
        for _ in 0..6 {
            Knob::ShadowCascades.step(&mut settings, -1);
        }
        assert_eq!(settings.shadow_cascades, 1);
        for _ in 0..6 {
            Knob::ShadowCascades.step(&mut settings, 1);
        }
        assert_eq!(settings.shadow_cascades, MAX_SHADOW_CASCADES);

        // The metering range never crosses.
        for _ in 0..100 {
            Knob::ExposureMin.step(&mut settings, 1);
        }
        assert!(settings.exposure_min < settings.exposure_max);
        for _ in 0..100 {
            Knob::ExposureMax.step(&mut settings, -1);
        }
        assert!(settings.exposure_min < settings.exposure_max);

        assert_eq!(settings.aa, AntiAliasing::Off);
        Knob::Aa.step(&mut settings, -1);
        assert_eq!(settings.aa, AntiAliasing::Taa, "a choice wraps round");
        Knob::Aa.step(&mut settings, 1);
        assert_eq!(settings.aa, AntiAliasing::Off);

        let start = settings.tonemapper;
        Knob::Tonemapper.step(&mut settings, 1);
        Knob::Tonemapper.step(&mut settings, -1);
        assert_eq!(settings.tonemapper, start, "previous undoes next");

        settings.shadow_distance = None;
        Knob::ShadowDistance.step(&mut settings, -1);
        assert_eq!(settings.shadow_distance, None, "stream is the bottom");
        Knob::ShadowDistance.step(&mut settings, 1);
        assert_eq!(settings.shadow_distance, Some(SHADOW_DISTANCES[0]));
        Knob::ShadowDistance.step(&mut settings, -1);
        assert_eq!(settings.shadow_distance, None);
        settings.shadow_distance = Some(3000.0);
        Knob::ShadowDistance.step(&mut settings, 1);
        assert_eq!(
            settings.shadow_distance,
            Some(4000.0),
            "off-list goes to the next"
        );
    }

    #[test]
    fn a_change_relabels_and_the_presets_switch_and_custom_comes_back() {
        let mut panel = panel_at(Knob::Aa);
        let mut settings = GraphicsSettings::current();
        panel.change(&mut settings, 1);
        assert_eq!(settings.aa, AntiAliasing::Fxaa);
        assert_eq!(settings.preset, Preset::Custom, "an edit reads as custom");
        let edited = settings.clone();

        let bevy = panel.preset_settings(Preset::Bevy);
        assert_eq!(bevy, GraphicsSettings::bevy());
        assert_eq!(
            panel.preset_settings(Preset::Current),
            GraphicsSettings::current()
        );
        assert_eq!(
            panel.preset_settings(Preset::Custom),
            edited,
            "3 brings back the run's own custom settings"
        );

        // Stepping back onto `current`'s own values reads as `current` again.
        let mut back = edited.clone();
        panel.change(&mut back, -1);
        assert_eq!(back, GraphicsSettings::current());

        let fresh = GraphicsPanel::default();
        let custom = fresh.preset_settings(Preset::Custom);
        assert_eq!(custom.preset, Preset::Custom);
        assert_eq!(
            GraphicsSettings {
                preset: Preset::Current,
                ..custom
            },
            GraphicsSettings::current(),
            "no custom settings yet: current, labelled custom"
        );
    }

    #[test]
    fn saved_settings_load_back_the_same() {
        let dir = std::env::temp_dir().join(format!("graphics-panel-{}", std::process::id()));
        let path = dir.join("nested").join("graphics.toml");
        let mut auto_high = GraphicsSettings::bevy();
        auto_high.ev100 = 11.5;
        auto_high.exposure_min = 9.0;
        auto_high.exposure_max = 12.0;
        auto_high.exposure_speed = 4.5;
        auto_high.exposure_speed_down = 0.5;
        auto_high.relabel();
        let mut low_range = GraphicsSettings::current();
        low_range.exposure_min = -15.0;
        low_range.exposure_max = -10.0;
        low_range.shadow_distance = Some(4000.0);
        low_range.tonemapper = Tonemapping::AgX;
        low_range.bloom = false;
        low_range.portal_scale = 0.35;
        low_range.shadow_filter = ShadowFilter::Temporal;
        low_range.relabel();
        let labelled_custom = Preset::Custom.settings();
        for settings in [
            GraphicsSettings::current(),
            GraphicsSettings::bevy(),
            labelled_custom,
            auto_high,
            low_range,
        ] {
            save(&path, &settings).unwrap();
            assert_eq!(load(&path).unwrap(), settings, "{}", settings.summary());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_panel_text_lists_every_knob_the_preset_and_the_flash_note() {
        let panel = panel_at(Knob::Tonemapper);
        let settings = GraphicsSettings::bevy();
        let text = panel_text(&panel, &settings);
        assert!(text.contains("Preset: bevy"), "{text}");
        for knob in Knob::ALL {
            assert!(text.contains(knob.label()), "{knob:?}");
        }
        assert!(
            text.contains("> tonemapper"),
            "the cursor is on the selected knob"
        );
        assert!(text.contains("TonyMcMapface"));
        assert!(text.contains(FLASH_NOTE));
        assert!(text.contains("local/graphics.toml"));
    }

    fn input_world(open: bool) -> World {
        let mut world = World::new();
        world.insert_resource(ButtonInput::<KeyCode>::default());
        world.insert_resource(ButtonInput::<MouseButton>::default());
        world.insert_resource(AccumulatedMouseMotion::default());
        world.insert_resource(GraphicsSettings::current());
        world.insert_resource(demo_hud::Notices::default());
        world.insert_resource(GraphicsPanel { open, ..default() });
        world
    }

    fn press(world: &mut World, key: KeyCode) {
        let mut keyboard = world.resource_mut::<ButtonInput<KeyCode>>();
        keyboard.clear();
        keyboard.release_all();
        keyboard.press(key);
    }

    #[test]
    fn g_opens_the_panel_and_its_keys_change_settings_and_are_blocked_from_the_game() {
        let mut world = input_world(false);
        press(&mut world, KeyCode::KeyW);
        world.run_system_once(panel_input).unwrap();
        assert!(
            world
                .resource::<ButtonInput<KeyCode>>()
                .pressed(KeyCode::KeyW),
            "closed: the game keeps its keys"
        );

        press(&mut world, TOGGLE_KEY);
        world.run_system_once(panel_input).unwrap();
        assert!(world.resource::<GraphicsPanel>().open);
        assert!(
            !world
                .resource::<ButtonInput<KeyCode>>()
                .just_pressed(TOGGLE_KEY),
            "the key that opened it is consumed"
        );

        press(&mut world, KeyCode::ArrowRight);
        world
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        world.resource_mut::<AccumulatedMouseMotion>().delta = Vec2::new(5.0, 3.0);
        world.run_system_once(panel_input).unwrap();
        assert_eq!(world.resource::<GraphicsSettings>().aa, AntiAliasing::Fxaa);
        assert!(
            !world
                .resource::<ButtonInput<KeyCode>>()
                .pressed(KeyCode::ArrowRight)
        );
        assert!(
            !world
                .resource::<ButtonInput<MouseButton>>()
                .pressed(MouseButton::Left)
        );
        assert_eq!(
            world.resource::<AccumulatedMouseMotion>().delta,
            Vec2::ZERO,
            "no mouse look while open"
        );

        press(&mut world, KeyCode::Digit2);
        world.run_system_once(panel_input).unwrap();
        assert_eq!(
            *world.resource::<GraphicsSettings>(),
            GraphicsSettings::bevy()
        );

        press(&mut world, KeyCode::Escape);
        world.run_system_once(panel_input).unwrap();
        assert!(!world.resource::<GraphicsPanel>().open, "Esc closes it");
        assert!(
            !world
                .resource::<ButtonInput<KeyCode>>()
                .just_pressed(KeyCode::Escape),
            "the Esc that closed it does not also release the mouse"
        );
    }
}
