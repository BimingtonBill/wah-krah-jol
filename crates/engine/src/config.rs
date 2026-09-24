use bevy::prelude::Resource;
use std::path::PathBuf;

#[derive(Debug, Clone, Resource)]
pub struct EngineConfig {
    pub assets_dir: PathBuf,
    pub worldspace_id: u32,
    pub start_grid: (i32, i32),
    pub stream_radius: i32,
    pub unload_radius: i32,
    /// How far the terrain-only ring reaches, in cells from the camera cell: a cell outside
    /// [`stream_radius`](Self::stream_radius) but within this distance streams its landscape and its
    /// water plane and nothing else, so the world does not end at the full-detail edge.
    ///
    /// A value at or below `stream_radius` leaves the ring empty, which is the engine without a
    /// ring (`--stream-radius 2` with the default) and what a benchmark run that wants the old
    /// numbers passes. Interior cells are never terrain-only.
    pub terrain_radius: i32,
    pub max_cell_commits_per_frame: usize,
    pub max_commit_micros_per_frame: u64,
    pub headless: bool,
    pub benchmark_only: bool,
    pub benchmark_frames: Option<u32>,
    pub benchmark_duration_secs: Option<f64>,
    pub benchmark_warmup_frames: u32,
    pub benchmark_output: PathBuf,
    pub accept_min_fps: f64,
    pub accept_p95_ms: f64,
    pub accept_max_memory_growth_gib: f64,
    pub auto_fly_speed: f32,
    pub allow_incomplete_assets: bool,
    pub synthetic_instances: usize,
    pub profile_output_dir: Option<PathBuf>,
    pub profile_scenario: String,
    pub profile_run_id: String,
    pub profile_commit: String,
    pub profile_dirty_worktree: bool,
    pub profile_hardware: String,
    pub acceptance_screenshot: Option<PathBuf>,
    pub diagnostic_asset_fallbacks: bool,
    pub material_fixture: bool,
    pub terrain_water_fixture: bool,
    pub transform_bounds_fixture: bool,
    pub renderer_fixture: bool,
    pub streaming_fixture: bool,
    /// Exact start in Creation-engine units, overriding the default camera placement.
    pub start_position: Option<[f32; 3]>,
    /// Start heading in Creation-engine radians (rotation about Z), used with start_position.
    pub start_yaw: f32,
    /// The portal's own options: the walk, the demo starts, the scripted tour and the shots run.
    /// One struct, parsed by one block at the end of this file ([`PortalOptions`]).
    pub portal: PortalOptions,
}

/// The default reach of the terrain-only ring, in cells: eight cells is 32,768 units of landscape
/// beyond the full-detail grid, drawn from 264 cells.
///
/// Chosen by measurement, on a `--shots` camera pose over the Pale from the Alftand ruins
/// (`far-pale-yaw40`), release build, one run each, warm:
///
/// | `terrain_radius` | ring cells | average FPS | 95th percentile frame |
/// |---|---|---|---|
/// | 2 (no ring) | 0 | 180 | 6.6 ms |
/// | 8 | 264 | 97 | 12.1 ms |
/// | 10 | 400 | 59 | 26.5 ms |
/// | 12 | 600 | 59 | 18.9 ms |
/// | 16 | 1,064 | 20 | 54.2 ms |
///
/// Each ring cell is four landscape draws of its own - one per quadrant, each with its own
/// material - and costs the frame about 20 µs, so a ring's price is its cell count. Eight is the
/// largest that stays clearly above the sixty-frame acceptance gate with the whole Pale in view;
/// ten and twelve sit on the gate and sixteen falls through it. Frames were measured while other
/// work ran on this machine and single runs differ by a quarter either way, so read the table as
/// order of magnitude rather than to the frame.
pub const DEFAULT_TERRAIN_RADIUS: i32 = 8;

/// The largest ring `--terrain-radius` accepts. A ring this wide is 4,225 cells and already more
/// landscape than a run holds; the clamp is a guard against a typo asking for millions of cells.
pub const MAX_TERRAIN_RADIUS: i32 = 32;

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            assets_dir: PathBuf::from("modern_assets"),
            worldspace_id: 0x3c,
            start_grid: (0, 0),
            stream_radius: 2,
            unload_radius: 3,
            terrain_radius: DEFAULT_TERRAIN_RADIUS,
            max_cell_commits_per_frame: 1,
            max_commit_micros_per_frame: 16_670,
            headless: false,
            benchmark_only: false,
            benchmark_frames: None,
            benchmark_duration_secs: None,
            benchmark_warmup_frames: 60,
            benchmark_output: PathBuf::from("benchmark-report.json"),
            accept_min_fps: 60.0,
            accept_p95_ms: 16.67,
            accept_max_memory_growth_gib: 0.5,
            auto_fly_speed: 0.0,
            allow_incomplete_assets: false,
            synthetic_instances: 250_000,
            profile_output_dir: None,
            profile_scenario: "adhoc".into(),
            profile_run_id: "run-1".into(),
            profile_commit: "unknown".into(),
            profile_dirty_worktree: false,
            profile_hardware: "unspecified".into(),
            acceptance_screenshot: None,
            diagnostic_asset_fallbacks: false,
            material_fixture: false,
            terrain_water_fixture: false,
            transform_bounds_fixture: false,
            renderer_fixture: false,
            streaming_fixture: false,
            start_position: None,
            start_yaw: 0.0,
            portal: PortalOptions::default(),
        }
    }
}

impl EngineConfig {
    pub fn from_env() -> Self {
        Self::from_args(std::env::args().skip(1))
    }

    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let mut config = Self::default();
        let mut terrain_radius_given = false;
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            // The portal's own flags, parsed by one block at the end of this file.
            if PortalOptions::parse_flag(&mut config, &argument, &mut args) {
                continue;
            }
            match argument.as_str() {
                "--assets" => {
                    if let Some(value) = args.next() {
                        config.assets_dir = value.into();
                    }
                }
                "--worldspace" => {
                    if let Some(value) = args.next().and_then(|value| parse_u32(&value)) {
                        config.worldspace_id = value;
                    }
                }
                "--grid-x" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.start_grid.0 = value;
                    }
                }
                "--grid-y" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.start_grid.1 = value;
                    }
                }
                "--stream-radius" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.stream_radius = value;
                        config.unload_radius = value + 1;
                    }
                }
                "--terrain-radius" => {
                    if let Some(value) = args.next().and_then(|value| value.parse::<i32>().ok()) {
                        config.terrain_radius = value.clamp(0, MAX_TERRAIN_RADIUS);
                        terrain_radius_given = true;
                    }
                }
                "--headless" => config.headless = true,
                "--max-commit-ms" => {
                    if let Some(value) = args.next().and_then(|value| value.parse::<f64>().ok())
                        && value.is_finite()
                        && value > 0.0
                    {
                        config.max_commit_micros_per_frame =
                            (value * 1_000.0).round().clamp(1.0, u64::MAX as f64) as u64;
                    }
                }
                "--benchmark-only" => config.benchmark_only = true,
                "--benchmark-frames" => {
                    config.benchmark_frames = args.next().and_then(|value| value.parse().ok());
                }
                "--benchmark-duration" => {
                    config.benchmark_duration_secs =
                        args.next().and_then(|value| value.parse().ok());
                }
                "--benchmark-warmup-frames" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.benchmark_warmup_frames = value;
                    }
                }
                "--benchmark-output" => {
                    if let Some(value) = args.next() {
                        config.benchmark_output = value.into();
                    }
                }
                "--accept-min-fps" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.accept_min_fps = value;
                    }
                }
                "--accept-p95-ms" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.accept_p95_ms = value;
                    }
                }
                "--accept-max-memory-growth-gib" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.accept_max_memory_growth_gib = value;
                    }
                }
                "--auto-fly-speed" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.auto_fly_speed = value;
                    }
                }
                "--allow-incomplete-assets" => config.allow_incomplete_assets = true,
                "--synthetic-instances" => {
                    if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                        config.synthetic_instances = value;
                    }
                }
                "--profile-output" => {
                    config.profile_output_dir = args.next().map(PathBuf::from);
                }
                "--profile-scenario" => {
                    if let Some(value) = args.next() {
                        config.profile_scenario = value;
                    }
                }
                "--profile-run-id" => {
                    if let Some(value) = args.next() {
                        config.profile_run_id = value;
                    }
                }
                "--profile-commit" => {
                    if let Some(value) = args.next() {
                        config.profile_commit = value;
                    }
                }
                "--profile-dirty-worktree" => config.profile_dirty_worktree = true,
                "--profile-hardware" => {
                    if let Some(value) = args.next() {
                        config.profile_hardware = value;
                    }
                }
                "--acceptance-screenshot" => {
                    config.acceptance_screenshot = args.next().map(PathBuf::from);
                }
                "--diagnostic-asset-fallbacks" => config.diagnostic_asset_fallbacks = true,
                "--material-fixture" => config.material_fixture = true,
                "--terrain-water-fixture" => config.terrain_water_fixture = true,
                "--transform-bounds-fixture" => config.transform_bounds_fixture = true,
                "--renderer-fixture" => config.renderer_fixture = true,
                "--streaming-fixture" => config.streaming_fixture = true,
                _ => {}
            }
        }
        // Benchmark and acceptance runs measure the full-detail grid the Phase 2 gates were set
        // for, so the terrain ring is off there unless a run asks for it by name.
        if !terrain_radius_given
            && (config.benchmark_frames.is_some() || config.benchmark_duration_secs.is_some())
        {
            config.terrain_radius = config.stream_radius;
        }
        config
    }
}

/// The portal's own options, and everything that reads them: the walk, the demo starts, the
/// scripted tour, the shots run, the start shot, and the run modes the wiring asks about.
///
/// One struct and one parser, so that a merge from `main` finds the whole of the portal's
/// configuration in one contiguous region - plus [`EngineConfig::portal`], its default, and the two
/// lines in [`EngineConfig::from_args`] that call [`PortalOptions::parse_flag`]
/// (docs/design/portal-plugin.md).
#[derive(Debug, Clone, Default)]
pub struct PortalOptions {
    /// Interactive first-person player (mouse look, walking, E opens load doors) instead of the
    /// free-flight camera. Never used by acceptance or benchmark runs.
    pub walk: bool,
    /// Scripted walk through the Alftand -> Blackreach doors, writing screenshots and a log here.
    pub demo_tour: Option<PathBuf>,
    /// `--tour-doors N`: walk only the first `N` doors of the `--demo-tour` route and stop. A
    /// smoke tour for iterating on the engine - it never prints the full tour's verdict word, so
    /// it cannot be mistaken for a sign-off - and inert unless `--demo-tour` names an output
    /// folder for it.
    pub tour_doors: Option<usize>,
    /// The --demo start that was chosen, if any (drives the on-screen objective).
    pub demo: Option<String>,
    /// Render each camera pose in this file to a PNG, then exit (see
    /// docs/design/reference-shots.md).
    pub shots: Option<PathBuf>,
    /// Where a shots run writes its images and `shots.log`. Defaults to a folder named after the
    /// shots file, next to it.
    pub shots_out: Option<PathBuf>,
    /// `--start-shot <shots.json> <name>`: start the run at a shot's pose, in its space
    /// (`crate::pose_capture`).
    pub start_shot: Option<StartShot>,
}

/// A `--start-shot` request as the command line wrote it: a shots file, and the name of the shot in
/// it to start the run at.
///
/// Both fields are optional because the flag's own parser cannot fail: `--start-shot examples.json`
/// with no name after it, and `--start-shot` at the end of the line, are requests nothing can carry
/// out. They are refused by name before the window exists, in
/// [`pose_capture::start_shot_run`](crate::pose_capture::start_shot_run), which is also where the
/// shot is read and where the flags that pose the camera beside this one are refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartShot {
    /// The shots file the shot is in.
    pub file: Option<PathBuf>,
    /// The shot's name, as the file writes it.
    pub name: Option<String>,
}

impl PortalOptions {
    /// Parses one portal flag, taking the arguments it needs from `args`, and says whether
    /// `argument` was one of them.
    ///
    /// It takes the whole configuration because two of these flags are not the portal's alone:
    /// `--demo` and `--start-position` write the worldspace, the start grid and the start pose a
    /// run streams and starts in, which the streaming plan reads directly (and which a nested
    /// struct must not swallow).
    pub fn parse_flag(
        config: &mut EngineConfig,
        argument: &str,
        args: &mut impl Iterator<Item = String>,
    ) -> bool {
        match argument {
            "--walk" => config.portal.walk = true,
            "--demo-tour" => config.portal.demo_tour = args.next().map(PathBuf::from),
            "--tour-doors" => {
                config.portal.tour_doors = args.next().and_then(|value| value.parse().ok());
            }
            "--shots" => config.portal.shots = args.next().map(PathBuf::from),
            "--shots-out" => config.portal.shots_out = args.next().map(PathBuf::from),
            // Both of the flag's arguments are taken, and either may be missing: what a request
            // that names no file, or no shot, is told is `pose_capture::start_shot_run`'s, which
            // is the first place that can say so with a message.
            "--start-shot" => {
                config.portal.start_shot = Some(StartShot {
                    file: args.next().map(PathBuf::from),
                    name: args.next(),
                });
            }
            "--demo" => {
                if let Some(name) = args.next()
                    && let Some(demo) = DemoStart::named(&name)
                {
                    demo.apply(config);
                    config.portal.demo = Some(name);
                }
            }
            "--start-position" => {
                let values: Vec<f32> = (0..3)
                    .filter_map(|_| args.next().and_then(|value| value.parse().ok()))
                    .collect();
                if let [x, y, z] = values[..] {
                    config.start_position = Some([x, y, z]);
                    config.start_grid = grid_of(x, y);
                }
            }
            "--start-yaw" => {
                if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                    config.start_yaw = value;
                }
            }
            _ => return false,
        }
        true
    }

    /// Where a `--shots` run writes its images and `shots.log`: the folder `--shots-out` names, or
    /// one named after the shots file, next to it. `None` when there is no shots run.
    pub fn shots_output_dir(&self) -> Option<PathBuf> {
        let path = self.shots.as_ref()?;
        Some(
            self.shots_out
                .clone()
                .unwrap_or_else(|| crate::shots::default_output_dir(path)),
        )
    }
}

impl EngineConfig {
    /// A run that walks: `--walk` with the camera left to the player's own controller. A
    /// benchmark, an auto-flight run, a `--shots` run and a `--start-shot` run keep the scripted
    /// camera whatever `--walk` was given, which is the rule `app::run` has always applied.
    pub fn walks(&self) -> bool {
        // A `--start-shot` run is driven by the player's own controller too - it starts in free
        // flight at the shot's pose (`crate::pose_capture`) - because the scripted fly camera has
        // no mouse look, and the person lining up a shot has to turn as well as move.
        (self.portal.walk || self.portal.start_shot.is_some())
            && self.benchmark_frames.is_none()
            && self.benchmark_duration_secs.is_none()
            && self.auto_fly_speed <= 0.0
            && self.portal.shots.is_none()
    }

    /// A run that is looked at rather than measured: sky and underground lighting, portals and
    /// lights, and no acceptance capture.
    ///
    /// A `--start-shot` run is one of these: it flies a camera of its own to a pose, which is what
    /// a measured run does not do.
    ///
    /// Both this and [`walks`](Self::walks) are pure functions of the configuration
    /// (`docs/design/portal-plugin.md`, H7), so the wiring that asks them - `app::run`'s lighting
    /// gate, `crate::portal::PortalPlugin`, `crate::demo_tour::DemoTourPlugin` - agrees whatever it
    /// is asked from.
    pub fn interactive(&self) -> bool {
        self.walks()
            || self.portal.demo_tour.is_some()
            || self.portal.shots.is_some()
            || self.portal.start_shot.is_some()
    }
}

/// Named starting points for interactive demos (--demo <name>). Positions are Creation-engine
/// units taken from Skyrim.esm (see docs/research/worldspace-transition-demo.md, section 2.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DemoStart {
    pub worldspace_id: u32,
    pub position: [f32; 3],
    pub yaw: f32,
}

impl DemoStart {
    pub fn named(name: &str) -> Option<Self> {
        match name {
            // Outside the Alftand entrance in the Pale, exactly where the game puts a player who walks
            // out of Alftand01: door 000152CF's XTEL arrival point and facing (out over the tundra;
            // the entrance, auto-load door 00015D48, is behind).
            "alftand" => Some(Self {
                worldspace_id: 0x3c,
                position: [77583.23, 77411.89, -5817.21],
                yaw: -0.603_385_3,
            }),
            // Straight into Blackreach: the arrival point of Alftand's lower door (0006998D).
            "blackreach" => Some(Self {
                worldspace_id: 0x0001_EE62,
                position: [21088.559, 18512.045, 2434.0],
                yaw: -1.870_80,
            }),
            // On the Helgen road south-west of Riverwood, looking up the street at Sven's house
            // and the village (docs/design/riverwood-demo.md, section 3). The position is the
            // Creation foot point on the road chunk 0002C699; the engine adds the eye height.
            "riverwood" => Some(Self {
                worldspace_id: 0x3c,
                position: [19600.0, -46650.0, -153.0],
                yaw: 1.336_0,
            }),
            _ => None,
        }
    }

    pub fn apply(self, config: &mut EngineConfig) {
        config.worldspace_id = self.worldspace_id;
        config.start_position = Some(self.position);
        config.start_yaw = self.yaw;
        config.start_grid = grid_of(self.position[0], self.position[1]);
    }
}

/// The exterior cell grid containing a Creation-engine position (cells are 4096 units).
pub fn grid_of(x: f32, y: f32) -> (i32, i32) {
    ((x / 4096.0).floor() as i32, (y / 4096.0).floor() as i32)
}

fn parse_u32(value: &str) -> Option<u32> {
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or_else(
            || value.parse().ok(),
            |hex| u32::from_str_radix(hex, 16).ok(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_one_cell_commit_within_a_sixty_fps_frame() {
        let config = EngineConfig::default();
        assert_eq!(config.max_cell_commits_per_frame, 1);
        assert_eq!(config.max_commit_micros_per_frame, 16_670);
    }

    #[test]
    fn defaults_to_a_terrain_ring_beyond_the_full_detail_grid() {
        let config = EngineConfig::default();
        assert_eq!(config.stream_radius, 2);
        assert_eq!(config.terrain_radius, DEFAULT_TERRAIN_RADIUS);
        assert!(
            config.terrain_radius > config.stream_radius,
            "the default ring is a second, wider tier around the full-detail grid"
        );
    }

    #[test]
    fn a_benchmark_run_keeps_the_ring_off_unless_asked() {
        let bench = EngineConfig::from_args(["--benchmark-frames", "100"].map(str::to_owned));
        assert_eq!(bench.terrain_radius, bench.stream_radius);
        let asked = EngineConfig::from_args(
            ["--benchmark-frames", "100", "--terrain-radius", "8"].map(str::to_owned),
        );
        assert_eq!(asked.terrain_radius, 8);
    }

    #[test]
    fn parses_and_clamps_the_terrain_radius() {
        let config = EngineConfig::from_args(["--terrain-radius", "16"].map(str::to_owned));
        assert_eq!(config.terrain_radius, 16);
        assert_eq!(
            config.stream_radius, 2,
            "the ring is a knob of its own, not a multiple of the full-detail grid"
        );

        // A ring at or inside the inner grid is empty: the engine as it was before the ring.
        let config = EngineConfig::from_args(
            ["--stream-radius", "5", "--terrain-radius", "2"].map(str::to_owned),
        );
        assert_eq!((config.stream_radius, config.unload_radius), (5, 6));
        assert_eq!(config.terrain_radius, 2);

        // A mistyped radius is clamped rather than asking the planner for millions of cells.
        assert_eq!(
            EngineConfig::from_args(["--terrain-radius", "100000"].map(str::to_owned))
                .terrain_radius,
            MAX_TERRAIN_RADIUS
        );
        assert_eq!(
            EngineConfig::from_args(["--terrain-radius", "-4"].map(str::to_owned)).terrain_radius,
            0
        );
        assert_eq!(
            EngineConfig::from_args(["--terrain-radius", "not-a-number"].map(str::to_owned))
                .terrain_radius,
            DEFAULT_TERRAIN_RADIUS,
            "an unparsable value leaves the default in place"
        );
    }

    #[test]
    fn demo_starts_set_worldspace_grid_and_position() {
        let config = EngineConfig::from_args(["--demo", "alftand", "--walk"].map(String::from));
        assert!(config.portal.walk);
        assert_eq!(config.worldspace_id, 0x3c);
        assert_eq!(config.start_grid, (18, 18));
        assert_eq!(config.start_position, Some([77583.23, 77411.89, -5817.21]));
        // Bethesda's own exit facing for Alftand01 -> Tamriel (door 000152CF's XTEL).
        assert_eq!(config.start_yaw, -0.603_385_3);
        assert_eq!(config.portal.demo.as_deref(), Some("alftand"));

        let config = EngineConfig::from_args(["--demo", "blackreach"].map(String::from));
        assert!(!config.portal.walk);
        assert_eq!(config.worldspace_id, 0x0001_EE62);
        assert_eq!(config.start_grid, (5, 4));

        let config = EngineConfig::from_args(["--demo", "nowhere"].map(String::from));
        assert_eq!(config.start_position, None);
        assert_eq!(config.worldspace_id, 0x3c);

        let config = EngineConfig::from_args(
            [
                "--start-position",
                "-100",
                "8200",
                "5",
                "--start-yaw",
                "1.5",
            ]
            .map(String::from),
        );
        assert_eq!(config.start_position, Some([-100.0, 8200.0, 5.0]));
        assert_eq!(config.start_grid, (-1, 2));
        assert_eq!(config.start_yaw, 1.5);
    }

    #[test]
    fn shots_options_parse_and_default_the_output_folder() {
        let config = EngineConfig::from_args(
            ["--assets", "converted", "--shots", "shots/tamriel.json"].map(str::to_owned),
        );
        assert_eq!(
            config.portal.shots,
            Some(PathBuf::from("shots/tamriel.json"))
        );
        assert_eq!(config.portal.shots_out, None);
        assert_eq!(
            config.portal.shots_output_dir(),
            Some(PathBuf::from("shots/tamriel")),
            "the default output folder is named after the shots file, next to it"
        );

        let config = EngineConfig::from_args(
            ["--shots", "a.json", "--shots-out", "shots/out"].map(str::to_owned),
        );
        assert_eq!(
            config.portal.shots_output_dir(),
            Some(PathBuf::from("shots/out"))
        );

        // Nothing writes anywhere unless there is a shots file to render.
        assert_eq!(EngineConfig::default().portal.shots, None);
        assert_eq!(EngineConfig::default().portal.shots_output_dir(), None);
    }

    #[test]
    fn start_shot_options_parse_a_file_and_a_shot_name() {
        let config = EngineConfig::from_args(
            [
                "--assets",
                "converted",
                "--start-shot",
                "tools/reference/riverwood_shots.json",
                "RW-04-inn-front",
            ]
            .map(str::to_owned),
        );
        assert_eq!(
            config.portal.start_shot,
            Some(StartShot {
                file: Some(PathBuf::from("tools/reference/riverwood_shots.json")),
                name: Some("RW-04-inn-front".to_owned()),
            })
        );
        // The flag is a start pose and nothing else: it starts no shots run, no tour and no walk.
        assert_eq!(config.portal.shots, None);
        assert_eq!(config.portal.demo_tour, None);
        assert!(!config.portal.walk);
        assert_eq!(config.start_position, None);

        // Either of the flag's two arguments may be absent, and a request nothing can carry out is
        // refused where a message can be written rather than by the parser
        // (`pose_capture::start_shot_run`).
        assert_eq!(
            EngineConfig::from_args(["--start-shot", "a.json"].map(str::to_owned))
                .portal
                .start_shot,
            Some(StartShot {
                file: Some(PathBuf::from("a.json")),
                name: None
            })
        );
        assert_eq!(
            EngineConfig::from_args(["--start-shot"].map(str::to_owned))
                .portal
                .start_shot,
            Some(StartShot {
                file: None,
                name: None
            })
        );
        assert_eq!(EngineConfig::default().portal.start_shot, None);
    }

    #[test]
    fn a_start_shot_run_is_looked_at_and_driven_by_the_player() {
        let config = EngineConfig::from_args(["--start-shot", "a.json", "shot"].map(str::to_owned));
        assert!(
            config.interactive(),
            "a start-shot run gets the lighting and the portal"
        );
        assert!(
            config.walks(),
            "the player's controller drives it (in free flight), so the person can look around"
        );

        // `--walk` beside it changes nothing: the controller is the player's either way.
        let walked = EngineConfig::from_args(
            ["--walk", "--start-shot", "a.json", "shot"].map(str::to_owned),
        );
        assert!(walked.interactive());
        assert!(walked.walks());

        // A measured run is a measured run: nothing about this flag makes a benchmark a walk.
        let measured = EngineConfig::from_args(
            ["--start-shot", "a.json", "shot", "--benchmark-frames", "10"].map(str::to_owned),
        );
        assert!(!measured.walks());
        assert!(measured.interactive());
    }

    #[test]
    fn a_smoke_tour_flag_names_how_many_doors_to_walk() {
        let config = EngineConfig::from_args(
            [
                "--demo",
                "riverwood",
                "--walk",
                "--demo-tour",
                "out",
                "--tour-doors",
                "1",
            ]
            .map(str::to_owned),
        );
        assert_eq!(config.portal.tour_doors, Some(1));
        assert_eq!(
            config.portal.demo_tour,
            Some(PathBuf::from("out")),
            "a smoke tour still needs the folder the full tour writes its log and shots to"
        );

        // Without the flag there is no smoke tour, and the route is walked whole.
        assert_eq!(EngineConfig::default().portal.tour_doors, None);
        assert_eq!(
            EngineConfig::from_args(["--demo-tour", "out"].map(str::to_owned))
                .portal
                .tour_doors,
            None
        );
    }

    #[test]
    fn parses_runtime_options() {
        let config = EngineConfig::from_args(
            [
                "--assets",
                "converted",
                "--worldspace",
                "0x3c",
                "--grid-x",
                "4",
                "--stream-radius",
                "5",
                "--headless",
                "--max-commit-ms",
                "8.5",
                "--profile-output",
                "profiles/run-1",
                "--profile-scenario",
                "stress",
                "--profile-run-id",
                "run-3",
                "--profile-commit",
                "abc123",
                "--profile-dirty-worktree",
                "--profile-hardware",
                "test-machine",
                "--acceptance-screenshot",
                "evidence/rural.png",
                "--diagnostic-asset-fallbacks",
                "--material-fixture",
                "--terrain-water-fixture",
                "--transform-bounds-fixture",
                "--renderer-fixture",
                "--streaming-fixture",
            ]
            .map(str::to_owned),
        );
        assert_eq!(config.assets_dir, PathBuf::from("converted"));
        assert_eq!(config.worldspace_id, 0x3c);
        assert_eq!(config.start_grid, (4, 0));
        assert_eq!((config.stream_radius, config.unload_radius), (5, 6));
        assert!(config.headless);
        assert_eq!(config.max_commit_micros_per_frame, 8_500);
        assert_eq!(
            config.profile_output_dir,
            Some(PathBuf::from("profiles/run-1"))
        );
        assert_eq!(config.profile_scenario, "stress");
        assert_eq!(config.profile_run_id, "run-3");
        assert_eq!(config.profile_commit, "abc123");
        assert!(config.profile_dirty_worktree);
        assert_eq!(config.profile_hardware, "test-machine");
        assert_eq!(
            config.acceptance_screenshot,
            Some(PathBuf::from("evidence/rural.png"))
        );
        assert!(config.diagnostic_asset_fallbacks);
        assert!(config.material_fixture);
        assert!(config.terrain_water_fixture);
        assert!(config.transform_bounds_fixture);
        assert!(config.renderer_fixture);
        assert!(config.streaming_fixture);
    }
}
