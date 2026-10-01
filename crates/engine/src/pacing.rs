//! Measurements for the streaming pacing rework: how long the world takes to load around the
//! camera, how far behind loading falls while flying fast, and (with `render_timing`) what the
//! worst frames spend on. Measurement only: nothing here changes what the engine loads, in what
//! order, or how fast.
//!
//! * **World ready** ([`WorldReadyInputs::is_ready`]): every cell of the stream window resident, no
//!   cell loading, no database request in flight, the model arming queue empty, and no model or
//!   terrain/water surface still pending. [`PacingTracker`] records the time and frame count from
//!   the first frame to the first frame that holds.
//! * **Jump** (`--benchmark-jump`): once the world is first ready the camera moves to the centre of
//!   another cell, and the time to be ready again is recorded the same way.
//! * **Lag at speed** (`--auto-fly-speed`): the horizontal distance from the camera at which each
//!   model finished loading ([`FlyLag`]). A model that completes close to the camera arrived late.

use crate::{
    app::benchmark_jump_position,
    config::EngineConfig,
    profiling::ProfilingState,
    streaming::{RenderOrigin, StreamingMetrics, StreamingWorld, window_center},
    world::{cache::CellCache, components::CELL_SIZE, components::StreamingCamera},
};
use bevy::prelude::*;
use serde::Serialize;
use std::time::Instant;

/// The most model-ready distances kept; a run that completes more models keeps the first ones.
const MAX_READY_DISTANCES: usize = 200_000;

/// What the world-ready predicate reads, taken from one frame's streaming state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorldReadyInputs {
    /// Cells in the stream window, `(2r + 1)^2`.
    pub window_cells: usize,
    /// Cells of that window resident at full detail.
    pub resident_window_cells: usize,
    pub loading_cells: usize,
    pub active_requests: usize,
    /// Models whose scene is loaded but not yet handed to the spawner.
    pub arming_queue_depth: usize,
    pub pending_asset_instances: usize,
    pub pending_surface_instances: usize,
}

impl WorldReadyInputs {
    /// Whether the world around the camera is fully loaded. Each condition alone holds it back.
    pub fn is_ready(&self) -> bool {
        self.window_cells > 0
            && self.resident_window_cells >= self.window_cells
            && self.loading_cells == 0
            && self.active_requests == 0
            && self.arming_queue_depth == 0
            && self.pending_asset_instances == 0
            && self.pending_surface_instances == 0
    }

    fn from_streaming(
        metrics: &StreamingMetrics,
        window_cells: usize,
        resident_window_cells: usize,
    ) -> Self {
        Self {
            window_cells,
            resident_window_cells,
            loading_cells: metrics.loading_cells,
            active_requests: metrics.active_requests,
            arming_queue_depth: metrics.arming_queue_depth,
            pending_asset_instances: metrics.pending_asset_instances,
            pending_surface_instances: metrics.pending_surface_instances,
        }
    }
}

/// When a state was first reached: milliseconds and frames since the start (or the jump).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mark {
    millis: f64,
    frames: u64,
}

#[derive(Debug, Clone, Copy)]
struct JumpState {
    started_millis: f64,
    started_frame: u64,
    ready: Option<Mark>,
}

/// Time-to-ready, jump and fly-lag measurements for one run.
#[derive(Resource, Default)]
pub struct PacingTracker {
    started: Option<Instant>,
    frames: u64,
    first_ready: Option<Mark>,
    jump: Option<JumpState>,
    ready_distances: Vec<f32>,
    ready_models_total: u64,
}

impl PacingTracker {
    /// Feeds one frame. `elapsed_millis` is the time since the first frame. Returns true on the
    /// frame the world is first ready when a jump is configured: the caller moves the camera now.
    pub fn observe(&mut self, elapsed_millis: f64, ready: bool, jump_configured: bool) -> bool {
        self.frames += 1;
        let frames = self.frames;
        if self.first_ready.is_none() {
            if !ready {
                return false;
            }
            self.first_ready = Some(Mark {
                millis: elapsed_millis,
                frames,
            });
            if jump_configured {
                self.jump = Some(JumpState {
                    started_millis: elapsed_millis,
                    started_frame: frames,
                    ready: None,
                });
                return true;
            }
            return false;
        }
        if let Some(jump) = &mut self.jump
            && jump.ready.is_none()
            // The planner has not seen the new camera position on the jump's own frame.
            && frames > jump.started_frame
            && ready
        {
            jump.ready = Some(Mark {
                millis: elapsed_millis - jump.started_millis,
                frames: frames - jump.started_frame,
            });
        }
        false
    }

    /// Records a model that finished loading `distance` units (horizontally) from the camera.
    pub fn record_model_ready(&mut self, distance: f32) {
        self.ready_models_total = self.ready_models_total.saturating_add(1);
        if distance.is_finite() && self.ready_distances.len() < MAX_READY_DISTANCES {
            self.ready_distances.push(distance);
        }
    }

    /// The report fields. The lag block is only reported for a run that flies
    /// (`--auto-fly-speed`).
    pub fn report(&self, config: &EngineConfig, peak_arming_queue_depth: usize) -> PacingReport {
        PacingReport {
            world_ready_reached: self.first_ready.is_some(),
            time_to_world_ready_ms: self.first_ready.map(|mark| mark.millis),
            frames_to_world_ready: self.first_ready.map(|mark| mark.frames),
            jump_target: config.benchmark_jump.map(|(x, y)| [x, y]),
            jump_issued: self.jump.is_some(),
            time_to_world_ready_after_jump_ms: self
                .jump
                .and_then(|jump| jump.ready)
                .map(|mark| mark.millis),
            frames_to_world_ready_after_jump: self
                .jump
                .and_then(|jump| jump.ready)
                .map(|mark| mark.frames),
            fly_lag: (config.auto_fly_speed > 0.0)
                .then(|| FlyLag::from_distances(&self.ready_distances, self.ready_models_total)),
            peak_arming_queue_depth,
        }
    }
}

/// Fields added to the benchmark report at its top level. `null` means "never happened": a world
/// that never became ready has `world_ready_reached: false` and no times.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct PacingReport {
    /// Whether the world-ready predicate ever held. False means the other times are absent.
    pub world_ready_reached: bool,
    /// Milliseconds from the first frame to the first frame the world was fully loaded.
    pub time_to_world_ready_ms: Option<f64>,
    pub frames_to_world_ready: Option<u64>,
    /// The `--benchmark-jump` target cell, when one was given.
    pub jump_target: Option<[i32; 2]>,
    /// Whether the jump happened (it waits for the world to be ready first).
    pub jump_issued: bool,
    /// Milliseconds from the jump to the next frame the world was fully loaded again.
    pub time_to_world_ready_after_jump_ms: Option<f64>,
    pub frames_to_world_ready_after_jump: Option<u64>,
    /// Present only when the run flew (`--auto-fly-speed`).
    pub fly_lag: Option<FlyLag>,
    /// The largest number of models waiting to be armed at once.
    pub peak_arming_queue_depth: usize,
}

/// How close to the camera models finished loading while flying: each model's horizontal distance
/// from the camera on the frame it became ready.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FlyLag {
    /// Models that finished loading during the run.
    pub models_ready: u64,
    /// Of those, how many finished within one cell (4096 units) of the camera.
    pub ready_within_one_cell: u64,
    /// The 5th percentile and the minimum of those distances, in units; null with no models.
    pub ready_distance_p5: Option<f32>,
    pub ready_distance_min: Option<f32>,
}

impl FlyLag {
    pub fn from_distances(distances: &[f32], models_ready: u64) -> Self {
        let mut sorted: Vec<f32> = distances.to_vec();
        sorted.sort_by(f32::total_cmp);
        Self {
            models_ready,
            ready_within_one_cell: sorted.iter().filter(|d| **d <= CELL_SIZE).count() as u64,
            ready_distance_p5: distance_percentile(&sorted, 0.05),
            ready_distance_min: sorted.first().copied(),
        }
    }
}

/// Nearest-rank percentile of an ascending list, `None` when it is empty.
pub fn distance_percentile(sorted: &[f32], percentile: f64) -> Option<f32> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    Some(sorted[index.min(sorted.len() - 1)])
}

/// Horizontal (render x/z plane) distance between two positions.
pub fn horizontal_distance(a: Vec3, b: Vec3) -> f32 {
    Vec2::new(a.x - b.x, a.z - b.z).length()
}

/// Evaluates the world-ready predicate each frame, runs the benchmark jump, and keeps the tracker.
/// Runs at the end of the streaming chain, after the planner and the readiness scans.
#[allow(clippy::too_many_arguments)]
pub(crate) fn track_world_ready(
    config: Res<EngineConfig>,
    origin: Res<RenderOrigin>,
    streaming: Res<StreamingWorld>,
    metrics: Res<StreamingMetrics>,
    cache: Res<CellCache>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut tracker: ResMut<PacingTracker>,
    mut profiler: ResMut<ProfilingState>,
) {
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    let started = *tracker.started.get_or_insert_with(Instant::now);
    let center = window_center(&config, camera.translation, origin.0);
    let (window_cells, resident) =
        streaming.window_residency(config.worldspace_id, center, config.stream_radius);
    let inputs = WorldReadyInputs::from_streaming(&metrics, window_cells, resident);
    let elapsed_millis = started.elapsed().as_secs_f64() * 1000.0;
    let ready = inputs.is_ready();
    let jump_now = tracker.observe(elapsed_millis, ready, config.benchmark_jump.is_some());
    if let Some(grid) = config.benchmark_jump.filter(|_| jump_now) {
        let position = benchmark_jump_position(&config, &cache, origin.0, grid);
        camera.translation = position;
        profiler.event(
            "pacing",
            format!("benchmark_jump_{}_{}", grid.0, grid.1),
            Some(elapsed_millis),
        );
        info!(?grid, ?position, elapsed_millis, "benchmark jump issued");
    }
    profiler.set_gauge("pacing/world_ready", f64::from(u8::from(ready)));
    if tracker
        .first_ready
        .is_some_and(|mark| mark.frames == tracker.frames)
    {
        profiler.event("pacing", "world_ready", Some(elapsed_millis));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_inputs() -> WorldReadyInputs {
        WorldReadyInputs {
            window_cells: 25,
            resident_window_cells: 25,
            ..default()
        }
    }

    #[test]
    fn a_fully_loaded_window_is_ready() {
        assert!(ready_inputs().is_ready());
    }

    #[test]
    fn each_condition_alone_holds_the_world_back() {
        let holds_back: [fn(&mut WorldReadyInputs); 7] = [
            |i| i.resident_window_cells = 24,
            |i| i.loading_cells = 1,
            |i| i.active_requests = 1,
            |i| i.arming_queue_depth = 1,
            |i| i.pending_asset_instances = 1,
            |i| i.pending_surface_instances = 1,
            |i| i.window_cells = 0,
        ];
        for (index, hold) in holds_back.iter().enumerate() {
            let mut inputs = ready_inputs();
            hold(&mut inputs);
            assert!(!inputs.is_ready(), "condition {index} did not hold it back");
        }
    }

    #[test]
    fn time_to_ready_is_the_first_frame_the_predicate_holds() {
        let mut tracker = PacingTracker::default();
        assert!(!tracker.observe(0.0, false, false));
        assert!(!tracker.observe(16.0, false, false));
        assert!(!tracker.observe(32.0, true, false));
        // A later dip does not move the first time.
        assert!(!tracker.observe(48.0, false, false));
        assert!(!tracker.observe(64.0, true, false));
        let report = tracker.report(&EngineConfig::default(), 3);
        assert!(report.world_ready_reached);
        assert_eq!(report.time_to_world_ready_ms, Some(32.0));
        assert_eq!(report.frames_to_world_ready, Some(3));
        assert!(!report.jump_issued);
        assert_eq!(report.peak_arming_queue_depth, 3);
        assert!(report.fly_lag.is_none());
    }

    #[test]
    fn a_world_that_never_becomes_ready_says_so() {
        let mut tracker = PacingTracker::default();
        for frame in 0..10 {
            tracker.observe(f64::from(frame) * 16.0, false, true);
        }
        let report = tracker.report(&EngineConfig::default(), 0);
        assert!(!report.world_ready_reached);
        assert_eq!(report.time_to_world_ready_ms, None);
        assert!(!report.jump_issued, "no jump before the first ready");
        assert_eq!(report.time_to_world_ready_after_jump_ms, None);
    }

    #[test]
    fn the_jump_fires_once_and_times_the_next_ready() {
        let config = EngineConfig {
            benchmark_jump: Some((3, 4)),
            ..default()
        };
        let mut tracker = PacingTracker::default();
        assert!(!tracker.observe(0.0, false, true));
        assert!(
            tracker.observe(100.0, true, true),
            "jump on the first ready"
        );
        // The jump's own frame does not count; the next frame's ready does.
        assert!(!tracker.observe(116.0, true, true));
        let early = tracker.report(&config, 0);
        assert!(early.jump_issued);
        assert_eq!(early.time_to_world_ready_after_jump_ms, Some(16.0));
        // Once recorded the time stays, and no second jump fires.
        assert!(!tracker.observe(500.0, true, true));
        let report = tracker.report(&config, 0);
        assert_eq!(report.time_to_world_ready_after_jump_ms, Some(16.0));
        assert_eq!(report.frames_to_world_ready_after_jump, Some(1));
        assert_eq!(report.jump_target, Some([3, 4]));
    }

    #[test]
    fn the_jump_waits_for_a_loading_world_to_settle() {
        let config = EngineConfig {
            benchmark_jump: Some((3, 4)),
            ..default()
        };
        let mut tracker = PacingTracker::default();
        assert!(tracker.observe(10.0, true, true));
        assert!(!tracker.observe(26.0, false, true));
        assert!(!tracker.observe(42.0, false, true));
        assert!(!tracker.observe(260.0, true, true));
        let report = tracker.report(&config, 0);
        assert_eq!(report.time_to_world_ready_after_jump_ms, Some(250.0));
        assert_eq!(report.frames_to_world_ready_after_jump, Some(3));
    }

    #[test]
    fn nearest_rank_percentile_of_distances() {
        assert_eq!(distance_percentile(&[], 0.05), None);
        assert_eq!(distance_percentile(&[7.0], 0.05), Some(7.0));
        let sorted: Vec<f32> = (1..=100).map(|n| n as f32).collect();
        assert_eq!(distance_percentile(&sorted, 0.05), Some(6.0));
        assert_eq!(distance_percentile(&sorted, 1.0), Some(100.0));
    }

    #[test]
    fn fly_lag_counts_models_inside_one_cell() {
        let mut tracker = PacingTracker::default();
        for distance in [9000.0, 100.0, 4096.0, 4097.0, 20_000.0, f32::NAN] {
            tracker.record_model_ready(distance);
        }
        let flying = EngineConfig {
            auto_fly_speed: 5000.0,
            ..default()
        };
        let lag = tracker.report(&flying, 0).fly_lag.expect("a flying run");
        assert_eq!(lag.models_ready, 6);
        assert_eq!(lag.ready_within_one_cell, 2);
        assert_eq!(lag.ready_distance_min, Some(100.0));
        // Five finite distances: nearest rank for the 5th percentile is the second.
        assert_eq!(lag.ready_distance_p5, Some(4096.0));
        // An empty run reports nulls rather than zeros.
        let empty = PacingTracker::default().report(&flying, 0).fly_lag.unwrap();
        assert_eq!(empty.ready_distance_min, None);
        assert_eq!(empty.ready_distance_p5, None);
        assert_eq!(empty.ready_within_one_cell, 0);
    }

    #[test]
    fn horizontal_distance_ignores_height() {
        assert_eq!(
            horizontal_distance(Vec3::new(0.0, 0.0, 0.0), Vec3::new(3.0, 500.0, -4.0)),
            5.0
        );
    }
}
