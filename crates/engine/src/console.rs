//! Developer console: the backtick key opens a command line that pauses the game.
//!
//! Commands live in a [`ConsoleRegistry`]; every plugin registers its own with
//! [`AppConsoleExt::add_console_command`]. Parsing, completion, the "did you mean" search and
//! the scrollback are pure functions so they are tested without a UI.

use std::sync::Arc;

use bevy::{
    input::{ButtonState, InputSystems, keyboard::KeyboardInput},
    prelude::*,
};

use crate::physics::{CursorCapture, MotionReset};

/// Scrollback lines kept in memory.
pub const SCROLLBACK_CAP: usize = 200;
/// Scrollback lines drawn on screen.
pub const VISIBLE_LINES: usize = 15;
/// A misspelt command is suggested a correction when it is at most this many edits away.
const SUGGESTION_DISTANCE: usize = 2;

/// Command handler: gets the whole world and the arguments after the command name.
type HandlerFn = dyn Fn(&mut World, &[&str]) -> Result<String, String> + Send + Sync;
pub type CommandHandler = Box<HandlerFn>;

/// One console command, as a plugin registers it.
pub struct ConsoleCommand {
    /// Canonical lowercase name.
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    /// One line shown by `help`.
    pub help: &'static str,
    pub handler: CommandHandler,
}

/// A registered command. The handler is shared so a command can run with `&mut World`.
pub struct RegisteredCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub help: &'static str,
    handler: Arc<HandlerFn>,
}

#[derive(Resource, Default)]
pub struct ConsoleRegistry {
    commands: Vec<RegisteredCommand>,
}

impl ConsoleRegistry {
    /// Add a command; a later command with the same name replaces the earlier one.
    pub fn register(&mut self, command: ConsoleCommand) {
        self.commands.retain(|c| c.name != command.name);
        self.commands.push(RegisteredCommand {
            name: command.name,
            aliases: command.aliases,
            usage: command.usage,
            help: command.help,
            handler: Arc::from(command.handler),
        });
    }

    /// Look a command up by name or alias, ignoring case.
    pub fn get(&self, name: &str) -> Option<&RegisteredCommand> {
        self.commands.iter().find(|c| {
            c.name.eq_ignore_ascii_case(name)
                || c.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
        })
    }

    /// Canonical command names, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.commands.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names
    }

    /// One block listing every command with its usage and one-line help.
    pub fn help_text(&self) -> String {
        let mut lines = Vec::new();
        for name in self.names() {
            if let Some(command) = self.get(name) {
                lines.push(format_help_line(command));
            }
        }
        lines.join("\n")
    }
}

fn format_help_line(command: &RegisteredCommand) -> String {
    let aliases = if command.aliases.is_empty() {
        String::new()
    } else {
        format!(" (alias: {})", command.aliases.join(", "))
    };
    format!("{} - {}{aliases}", command.usage, command.help)
}

/// Console state shared by the input, dispatch and UI systems.
#[derive(Resource, Default)]
pub struct ConsoleState {
    pub open: bool,
    pub buffer: String,
    pub scrollback: Vec<String>,
    /// Cursor capture the player had when the console opened.
    pub previous_capture: CursorCapture,
    /// Whether virtual time was already paused when the console opened.
    pub was_paused: bool,
    /// Lines entered with Enter, run by the dispatch system.
    pub pending: Vec<String>,
}

/// Run condition: true while the console is closed, and when there is no console at all.
pub fn console_closed(state: Option<Res<ConsoleState>>) -> bool {
    state.is_none_or(|state| !state.open)
}

pub trait AppConsoleExt {
    fn add_console_command(&mut self, command: ConsoleCommand) -> &mut Self;
}

impl AppConsoleExt for App {
    fn add_console_command(&mut self, command: ConsoleCommand) -> &mut Self {
        self.init_resource::<ConsoleRegistry>();
        self.world_mut()
            .resource_mut::<ConsoleRegistry>()
            .register(command);
        self
    }
}

pub struct ConsolePlugin;

impl Plugin for ConsolePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConsoleRegistry>()
            .init_resource::<ConsoleState>()
            .init_resource::<CursorCapture>()
            .add_message::<KeyboardInput>()
            .add_console_command(help_command())
            .add_console_command(clear_command())
            .add_systems(Startup, spawn_console_ui)
            .add_systems(
                PreUpdate,
                (console_input_system, console_dispatch_system)
                    .chain()
                    .after(InputSystems),
            )
            .add_systems(Update, refresh_console_ui);
    }
}

fn help_command() -> ConsoleCommand {
    ConsoleCommand {
        name: "help",
        aliases: &[],
        usage: "help [command]",
        help: "list the commands, or describe one",
        handler: Box::new(|world, args| {
            let registry = world.resource::<ConsoleRegistry>();
            match args {
                [] => Ok(registry.help_text()),
                [name] => match registry.get(name) {
                    Some(command) => Ok(format_help_line(command)),
                    None => Err(unknown_command_message(name, &registry.names())),
                },
                _ => Err("usage: help [command]".to_owned()),
            }
        }),
    }
}

fn clear_command() -> ConsoleCommand {
    ConsoleCommand {
        name: "clear",
        aliases: &[],
        usage: "clear",
        help: "clear the scrollback",
        handler: Box::new(|world, _| {
            world.resource_mut::<ConsoleState>().scrollback.clear();
            Ok(String::new())
        }),
    }
}

// ---------------------------------------------------------------------------
// Pure helpers.
// ---------------------------------------------------------------------------

/// Split a line into the command name as typed and its arguments; `None` for a blank line.
pub fn parse_line(line: &str) -> Option<(&str, Vec<&str>)> {
    let mut tokens = line.split_whitespace();
    let name = tokens.next()?;
    Some((name, tokens.collect()))
}

/// Edit distance between two strings.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(above + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// The closest name within [`SUGGESTION_DISTANCE`] edits, if any.
pub fn nearest(input: &str, names: &[&str]) -> Option<String> {
    let input = input.to_ascii_lowercase();
    names
        .iter()
        .map(|name| (levenshtein(&input, name), *name))
        .filter(|(distance, _)| *distance <= SUGGESTION_DISTANCE)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name.to_owned())
}

pub fn unknown_command_message(input: &str, names: &[&str]) -> String {
    match nearest(input, names) {
        Some(near) => format!(
            "unknown command \"{input}\", did you mean \"{near}\"? Type \"help\" for the list."
        ),
        None => format!("unknown command \"{input}\". Type \"help\" for the list."),
    }
}

/// Result of completing the first token.
#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    /// Nothing starts with the token.
    None,
    /// Exactly one candidate.
    Unique(String),
    /// Several candidates sharing a longer prefix than the token.
    Prefix(String),
    /// Several candidates with nothing more in common: list them.
    List(Vec<String>),
}

pub fn completion(token: &str, names: &[&str]) -> Completion {
    let token = token.to_ascii_lowercase();
    if names.contains(&token.as_str()) {
        return Completion::Unique(token);
    }
    let matches: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| name.starts_with(&token))
        .collect();
    match matches.as_slice() {
        [] => Completion::None,
        [only] => Completion::Unique((*only).to_owned()),
        [first, rest @ ..] => {
            let mut prefix: Vec<char> = first.chars().collect();
            for name in rest {
                let common = prefix
                    .iter()
                    .zip(name.chars())
                    .take_while(|(a, b)| **a == *b)
                    .count();
                prefix.truncate(common);
            }
            let prefix: String = prefix.into_iter().collect();
            if prefix.len() > token.len() {
                Completion::Prefix(prefix)
            } else {
                Completion::List(matches.iter().map(|m| (*m).to_owned()).collect())
            }
        }
    }
}

/// Apply Tab to an input line. Returns the new line and the candidates to list, if any.
pub fn autofill(buffer: &str, names: &[&str]) -> (String, Vec<String>) {
    // Leading whitespace the user typed is kept.
    let token = buffer.trim_start();
    let indent = &buffer[..buffer.len() - token.len()];
    if token.contains(char::is_whitespace) {
        return (buffer.to_owned(), Vec::new());
    }
    match completion(token, names) {
        Completion::None => (buffer.to_owned(), Vec::new()),
        Completion::Unique(name) => (format!("{indent}{name} "), Vec::new()),
        Completion::Prefix(prefix) => (format!("{indent}{prefix}"), Vec::new()),
        Completion::List(list) => (buffer.to_owned(), list),
    }
}

/// Append text (possibly several lines) to the scrollback, dropping the oldest beyond the cap.
pub fn push_scrollback(lines: &mut Vec<String>, text: &str) {
    lines.extend(text.lines().map(str::to_owned));
    if lines.len() > SCROLLBACK_CAP {
        lines.drain(..lines.len() - SCROLLBACK_CAP);
    }
}

pub fn visible_scrollback(lines: &[String]) -> String {
    let start = lines.len().saturating_sub(VISIBLE_LINES);
    lines[start..].join("\n")
}

/// Insert typed text, dropping control characters.
pub fn insert_text(buffer: &mut String, text: &str) {
    buffer.extend(text.chars().filter(|c| !c.is_control()));
}

/// Run one entered line: echo it, find the command, show its output or error.
pub fn execute_line(world: &mut World, line: &str) {
    let line = line.trim();
    let Some((name, args)) = parse_line(line) else {
        return;
    };
    push_output(world, &format!("> {line}"));
    let (handler, names) = {
        let registry = world.resource::<ConsoleRegistry>();
        (
            registry.get(name).map(|c| Arc::clone(&c.handler)),
            registry.names(),
        )
    };
    let output = match handler {
        Some(handler) => match handler(world, &args) {
            Ok(text) => text,
            Err(text) => format!("error: {text}"),
        },
        None => unknown_command_message(name, &names),
    };
    if !output.is_empty() {
        push_output(world, &output);
    }
}

fn push_output(world: &mut World, text: &str) {
    if let Some(mut state) = world.get_resource_mut::<ConsoleState>() {
        push_scrollback(&mut state.scrollback, text);
    }
}

// ---------------------------------------------------------------------------
// Systems.
// ---------------------------------------------------------------------------

/// Backtick toggle and text entry. Runs after Bevy's input systems so every later system
/// sees the current frame's open state.
#[allow(clippy::too_many_arguments)]
fn console_input_system(
    keys: Res<ButtonInput<KeyCode>>,
    mut events: MessageReader<KeyboardInput>,
    mut state: ResMut<ConsoleState>,
    mut capture: ResMut<CursorCapture>,
    mut time: ResMut<Time<Virtual>>,
    registry: Res<ConsoleRegistry>,
    mut motion: MotionReset,
) {
    // The physical key under Escape on every layout (the same key Skyrim uses), not the character.
    if keys.just_pressed(KeyCode::Backquote) {
        if state.open {
            close_console(&mut state, &mut capture, &mut time, false);
        } else {
            state.open = true;
            state.buffer.clear();
            state.previous_capture = *capture;
            state.was_paused = time.is_paused();
            *capture = CursorCapture::Released;
            time.pause();
            // A key held across the pause must not resume as a stuck walk.
            motion.clear();
        }
    }
    for event in events.read() {
        if !state.open
            || event.state != ButtonState::Pressed
            || event.key_code == KeyCode::Backquote
        {
            continue;
        }
        match event.key_code {
            KeyCode::Escape => close_console(&mut state, &mut capture, &mut time, true),
            KeyCode::Backspace => {
                state.buffer.pop();
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                let line = std::mem::take(&mut state.buffer);
                state.pending.push(line);
            }
            KeyCode::Tab => {
                let (buffer, list) = autofill(&state.buffer, &registry.names());
                state.buffer = buffer;
                if !list.is_empty() {
                    push_scrollback(&mut state.scrollback, &list.join("  "));
                }
            }
            _ => {
                if let Some(text) = &event.text {
                    insert_text(&mut state.buffer, text);
                }
            }
        }
    }
}

/// Close the console. Escape leaves the cursor released, as it did before the console existed;
/// the backtick restores the capture the player had.
fn close_console(
    state: &mut ConsoleState,
    capture: &mut CursorCapture,
    time: &mut Time<Virtual>,
    escape: bool,
) {
    state.open = false;
    state.buffer.clear();
    *capture = if escape {
        CursorCapture::Released
    } else {
        state.previous_capture
    };
    if !state.was_paused {
        time.unpause();
    }
}

fn console_dispatch_system(world: &mut World) {
    // Check through `resource` first so an idle console is not marked changed every frame.
    if world.resource::<ConsoleState>().pending.is_empty() {
        return;
    }
    let lines = std::mem::take(&mut world.resource_mut::<ConsoleState>().pending);
    for line in lines {
        execute_line(world, &line);
    }
}

#[derive(Component)]
struct ConsoleRoot;
#[derive(Component)]
struct ConsoleScrollText;
#[derive(Component)]
struct ConsoleInputText;

fn spawn_console_ui(mut commands: Commands) {
    commands
        .spawn((
            Name::new("Developer console"),
            ConsoleRoot,
            Node {
                display: Display::None,
                position_type: PositionType::Absolute,
                bottom: Val::Px(0.0),
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(40.0),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::FlexEnd,
                padding: UiRect::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.75)),
            GlobalZIndex(1000),
        ))
        .with_children(|root| {
            root.spawn((
                ConsoleScrollText,
                Text::new(""),
                TextFont::from_font_size(16.0),
                TextColor(Color::WHITE),
            ));
            root.spawn((
                ConsoleInputText,
                Text::new("> "),
                TextFont::from_font_size(18.0),
                TextColor(Color::srgb(1.0, 0.9, 0.5)),
            ));
        });
}

#[allow(clippy::type_complexity)]
fn refresh_console_ui(
    state: Res<ConsoleState>,
    mut root: Query<&mut Node, With<ConsoleRoot>>,
    mut scroll: Query<&mut Text, (With<ConsoleScrollText>, Without<ConsoleInputText>)>,
    mut input: Query<&mut Text, (With<ConsoleInputText>, Without<ConsoleScrollText>)>,
) {
    if !state.is_changed() {
        return;
    }
    for mut node in &mut root {
        node.display = if state.open {
            Display::Flex
        } else {
            Display::None
        };
    }
    for mut text in &mut scroll {
        **text = visible_scrollback(&state.scrollback);
    }
    for mut text in &mut input {
        **text = format!("> {}", state.buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::input::keyboard::Key;
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    const NAMES: &[&str] = &["clear", "collision", "grab", "help", "noclip", "tankard"];

    fn console_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<ButtonInput<KeyCode>>()
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
                20,
            )))
            .add_plugins(ConsolePlugin);
        app.finish();
        app.update();
        app
    }

    fn press_backquote(app: &mut App) {
        let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        keys.press(KeyCode::Backquote);
        app.update();
        let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        keys.release(KeyCode::Backquote);
        keys.clear();
    }

    fn type_key(app: &mut App, key_code: KeyCode, logical_key: Key, text: Option<&str>) {
        app.world_mut().write_message(KeyboardInput {
            key_code,
            logical_key,
            state: ButtonState::Pressed,
            text: text.map(Into::into),
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        app.update();
    }

    fn type_char(app: &mut App, key_code: KeyCode, c: &str) {
        type_key(app, key_code, Key::Character(c.into()), Some(c));
    }

    #[test]
    fn parse_line_splits_name_and_args() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   \t "), None);
        assert_eq!(
            parse_line("  Help  noclip  "),
            Some(("Help", vec!["noclip"]))
        );
        assert_eq!(parse_line("a b c"), Some(("a", vec!["b", "c"])));
    }

    #[test]
    fn registry_lookup_is_case_insensitive_and_knows_aliases() {
        let mut registry = ConsoleRegistry::default();
        registry.register(ConsoleCommand {
            name: "noclip",
            aliases: &["v"],
            usage: "noclip",
            help: "toggle",
            handler: Box::new(|_, _| Ok(String::new())),
        });
        assert!(registry.get("NoClip").is_some());
        assert_eq!(registry.get("V").map(|c| c.name), Some("noclip"));
        assert!(registry.get("fly").is_none());
        assert_eq!(registry.names(), vec!["noclip"]);
    }

    #[test]
    fn completion_covers_none_unique_prefix_and_list() {
        assert_eq!(completion("zz", NAMES), Completion::None);
        assert_eq!(completion("n", NAMES), Completion::Unique("noclip".into()));
        assert_eq!(
            completion("ta", NAMES),
            Completion::Unique("tankard".into())
        );
        assert_eq!(
            completion("c", NAMES),
            Completion::List(vec!["clear".into(), "collision".into()])
        );
        assert_eq!(
            completion("col", NAMES),
            Completion::Unique("collision".into())
        );
        assert_eq!(
            completion("c", &["calc", "calm", "cold"]),
            Completion::List(vec!["calc".into(), "calm".into(), "cold".into()])
        );
        assert_eq!(
            completion("ca", &["calc", "calm", "cold"]),
            Completion::Prefix("cal".into())
        );
        assert_eq!(completion("help", NAMES), Completion::Unique("help".into()));
    }

    #[test]
    fn autofill_completes_only_the_first_token() {
        assert_eq!(autofill("noc", NAMES), ("noclip ".to_owned(), vec![]));
        assert_eq!(autofill("noclip foo", NAMES).0, "noclip foo");
        let (buffer, list) = autofill("c", NAMES);
        assert_eq!(buffer, "c");
        assert_eq!(list, vec!["clear", "collision"]);
        assert_eq!(autofill("qq", NAMES).0, "qq");
        assert_eq!(autofill("  noc", NAMES).0, "  noclip ");
        let wide = ["café", "cafétéria"];
        assert_eq!(completion("c", &wide), Completion::Prefix("café".into()));
        assert_eq!(completion("ca", &["éa", "éb"]), Completion::None);
        assert_eq!(
            completion("", &["éa", "éb"]),
            Completion::Prefix("é".into())
        );
    }

    #[test]
    fn levenshtein_and_suggestion() {
        assert_eq!(levenshtein("noclip", "noclip"), 0);
        assert_eq!(levenshtein("noclup", "noclip"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(
            unknown_command_message("noclup", NAMES),
            "unknown command \"noclup\", did you mean \"noclip\"? Type \"help\" for the list."
        );
        let none = unknown_command_message("zzzzzzzz", NAMES);
        assert_eq!(
            none,
            "unknown command \"zzzzzzzz\". Type \"help\" for the list."
        );
        assert_eq!(nearest("NOCLUP", NAMES).as_deref(), Some("noclip"));
        assert_eq!(nearest("qqqqq", NAMES), None);
    }

    #[test]
    fn scrollback_is_capped_and_multiline_text_splits() {
        let mut lines = Vec::new();
        for i in 0..SCROLLBACK_CAP + 25 {
            push_scrollback(&mut lines, &format!("line {i}"));
        }
        assert_eq!(lines.len(), SCROLLBACK_CAP);
        assert_eq!(lines[0], "line 25");
        push_scrollback(&mut lines, "a\nb");
        assert_eq!(lines.len(), SCROLLBACK_CAP);
        assert_eq!(lines.last().map(String::as_str), Some("b"));
        let shown = visible_scrollback(&lines);
        assert_eq!(shown.lines().count(), VISIBLE_LINES);
    }

    #[test]
    fn insert_text_drops_control_characters() {
        let mut buffer = String::new();
        insert_text(&mut buffer, "ab\r\t");
        insert_text(&mut buffer, "c");
        assert_eq!(buffer, "abc");
    }

    #[test]
    fn console_closed_is_true_without_the_resource() {
        let mut world = World::new();
        assert!(world.run_system_once(console_closed).unwrap());
        world.insert_resource(ConsoleState::default());
        assert!(world.run_system_once(console_closed).unwrap());
        world.resource_mut::<ConsoleState>().open = true;
        assert!(!world.run_system_once(console_closed).unwrap());
    }

    #[test]
    fn help_lists_every_command_and_clear_empties_the_scrollback() {
        let mut app = console_app();
        app.add_console_command(ConsoleCommand {
            name: "probe",
            aliases: &["p"],
            usage: "probe",
            help: "a test command",
            handler: Box::new(|_, _| Ok("probed".to_owned())),
        });
        execute_line(app.world_mut(), "help");
        let text = app.world().resource::<ConsoleState>().scrollback.join("\n");
        for command in [
            "help [command]",
            "clear",
            "probe - a test command (alias: p)",
        ] {
            assert!(text.contains(command), "help missing {command}: {text}");
        }
        execute_line(app.world_mut(), "help probe");
        execute_line(app.world_mut(), "help nope");
        let text = app.world().resource::<ConsoleState>().scrollback.join("\n");
        assert!(text.contains("error: unknown command \"nope\""));
        execute_line(app.world_mut(), "clear");
        assert!(app.world().resource::<ConsoleState>().scrollback.is_empty());
    }

    #[test]
    fn unknown_command_runs_nothing() {
        let mut app = console_app();
        app.add_console_command(ConsoleCommand {
            name: "sentinel",
            aliases: &[],
            usage: "sentinel",
            help: "must not run",
            handler: Box::new(|_, _| panic!("sentinel ran")),
        });
        execute_line(app.world_mut(), "sentinal");
        let text = app.world().resource::<ConsoleState>().scrollback.join("\n");
        assert!(text.contains("did you mean \"sentinel\""), "{text}");
    }

    #[test]
    fn typed_backtick_never_reaches_the_buffer() {
        let mut app = console_app();
        // The opening press arrives as a keyboard message too.
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Backquote);
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Backquote,
            logical_key: Key::Character("`".into()),
            state: ButtonState::Pressed,
            text: Some("`".into()),
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        app.update();
        let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        keys.release(KeyCode::Backquote);
        keys.clear();
        assert!(app.world().resource::<ConsoleState>().open);
        type_char(&mut app, KeyCode::KeyH, "h");
        type_key(
            &mut app,
            KeyCode::Backquote,
            Key::Character("`".into()),
            Some("`"),
        );
        type_char(&mut app, KeyCode::KeyI, "i");
        assert_eq!(app.world().resource::<ConsoleState>().buffer, "hi");
        type_key(&mut app, KeyCode::Backspace, Key::Backspace, None);
        assert_eq!(app.world().resource::<ConsoleState>().buffer, "h");
    }

    #[test]
    fn enter_runs_the_line_and_tab_autofills() {
        let mut app = console_app();
        press_backquote(&mut app);
        for (code, c) in [(KeyCode::KeyC, "c"), (KeyCode::KeyL, "l")] {
            type_char(&mut app, code, c);
        }
        type_key(&mut app, KeyCode::Tab, Key::Tab, None);
        assert_eq!(app.world().resource::<ConsoleState>().buffer, "clear ");
        type_key(&mut app, KeyCode::Enter, Key::Enter, None);
        let state = app.world().resource::<ConsoleState>();
        assert!(state.buffer.is_empty() && state.pending.is_empty());
        assert!(
            state.scrollback.is_empty(),
            "clear emptied it, got {:?}",
            state.scrollback
        );
    }

    #[derive(Resource, Default)]
    struct Probe(u32);

    fn probe_app() -> App {
        let mut app = console_app();
        app.init_resource::<Probe>()
            .add_systems(FixedUpdate, |mut probe: ResMut<Probe>| probe.0 += 1);
        app
    }

    #[test]
    fn open_pauses_virtual_time_and_close_resumes() {
        let mut app = probe_app();
        for _ in 0..5 {
            app.update();
        }
        assert!(app.world().resource::<Probe>().0 > 0, "probe never ran");
        press_backquote(&mut app);
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
        app.update();
        let frozen = app.world().resource::<Probe>().0;
        for _ in 0..10 {
            app.update();
        }
        assert_eq!(app.world().resource::<Probe>().0, frozen);
        press_backquote(&mut app);
        assert!(!app.world().resource::<Time<Virtual>>().is_paused());
        for _ in 0..10 {
            app.update();
        }
        assert!(app.world().resource::<Probe>().0 > frozen);
    }

    #[test]
    fn close_keeps_a_pause_that_predates_the_console() {
        let mut app = console_app();
        app.world_mut().resource_mut::<Time<Virtual>>().pause();
        press_backquote(&mut app);
        assert!(app.world().resource::<ConsoleState>().was_paused);
        press_backquote(&mut app);
        assert!(!app.world().resource::<ConsoleState>().open);
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
    }

    #[test]
    fn open_releases_the_cursor_and_close_restores_it() {
        let mut app = console_app();
        app.insert_resource(CursorCapture::Captured);
        press_backquote(&mut app);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
        press_backquote(&mut app);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Captured
        );
        press_backquote(&mut app);
        type_key(&mut app, KeyCode::Escape, Key::Escape, None);
        assert!(!app.world().resource::<ConsoleState>().open);
        assert_eq!(
            *app.world().resource::<CursorCapture>(),
            CursorCapture::Released
        );
    }
}
