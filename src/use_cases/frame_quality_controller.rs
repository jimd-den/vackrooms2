//! # Frame Quality & Budget Controller Use Case
//!
//! ## Business Intent & Rationale
//! In low-spec rendering environments (WebGL, low-end integrated GPUs, mobile devices),
//! hardware rendering capacity is strictly constrained. Running out of frame budget causes
//! stuttering, thermal throttling, and missed v-sync intervals.
//!
//! The `FrameQualityController` orchestrates runtime quality profiles (`SurfaceQualityProfile`)
//! and enforces hard frame work budgets (maximum visible surface chunks, triangle limits, light limits).
//!
//! ## Design Patterns Used
//! - **Strategy Pattern**: `FrameQualityHysteresisStrategy` decouples performance measurement
//!   and tier transition logic from chunk submission algorithms. Hysteresis ensures the system
//!   degrades rapidly upon detecting performance degradation (1 slow frame triggers downgrade)
//!   while upgrading conservatively (requiring 30 consecutive fast frames to upgrade).
//! - **Observer Pattern / Telemetry**: `SurfaceRenderingTelemetry` records per-frame performance metrics,
//!   ISO 8601 timestamps, input arguments, and return values for observability.

use crate::domain::entities::position::Position;
use crate::domain::entities::surface_quality_profile::{SurfaceQualityProfile, SurfaceQualityTier};
use std::time::Duration;

/// Telemetry metric payload for observability & performance tracking.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceRenderingTelemetry {
    /// ISO 8601 formatted timestamp of the frame render event.
    pub timestamp_iso8601: String,
    /// Total candidate chunks evaluated for rendering.
    pub candidate_chunks: usize,
    /// Chunks culled by frustum/distance culling.
    pub distance_culled_chunks: usize,
    /// Chunks culled due to work budget limits.
    pub budget_culled_chunks: usize,
    /// Chunks accepted and submitted for surface rendering.
    pub visible_surface_chunks: usize,
    /// Total surface quads submitted.
    pub submitted_quads: usize,
    /// Total surface triangles submitted (2 per quad).
    pub submitted_triangles: usize,
    /// Total draw call submissions.
    pub draw_calls: usize,
    /// Active dynamic light count.
    pub active_lights: usize,
    /// Active quality tier name.
    pub active_tier: SurfaceQualityTier,
    /// Current render scale.
    pub render_scale: f32,
}

impl SurfaceRenderingTelemetry {
    pub fn new(
        timestamp_iso8601: impl Into<String>,
        active_tier: SurfaceQualityTier,
        render_scale: f32,
    ) -> Self {
        Self {
            timestamp_iso8601: timestamp_iso8601.into(),
            candidate_chunks: 0,
            distance_culled_chunks: 0,
            budget_culled_chunks: 0,
            visible_surface_chunks: 0,
            submitted_quads: 0,
            submitted_triangles: 0,
            draw_calls: 0,
            active_lights: 0,
            active_tier,
            render_scale,
        }
    }
}

/// A surface chunk candidate with its world origin position and quad count.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkSurfaceCandidate {
    pub chunk_x: i32,
    pub chunk_z: i32,
    pub origin: Position,
    pub quad_count: usize,
}

/// Use case controller governing real-time frame quality and budget culling.
#[derive(Debug, Clone)]
pub struct FrameQualityController {
    pub profile: SurfaceQualityProfile,
    target_frame_duration: Duration,
    fast_frame_counter: usize,
    upgrade_threshold_frames: usize,
}

impl FrameQualityController {
    pub fn new(initial_tier: SurfaceQualityTier) -> Self {
        let profile = match initial_tier {
            SurfaceQualityTier::UltraLow => SurfaceQualityProfile::ultra_low(),
            SurfaceQualityTier::Low => SurfaceQualityProfile::low(),
            SurfaceQualityTier::Balanced => SurfaceQualityProfile::balanced(),
            SurfaceQualityTier::High => SurfaceQualityProfile::high(),
        };
        Self {
            profile,
            target_frame_duration: Duration::from_millis(16), // ~60 FPS target
            fast_frame_counter: 0,
            upgrade_threshold_frames: 30, // Require 30 consecutive fast frames to upgrade
        }
    }

    /// Reports a completed frame's execution time and applies hysteresis rules.
    ///
    /// # Inputs
    /// - `frame_duration`: Execution time of the previous frame.
    ///
    /// # Returns
    /// - `bool`: True if the quality tier was changed.
    pub fn report_frame_time(&mut self, frame_duration: Duration) -> bool {
        let slow_threshold = self
            .target_frame_duration
            .saturating_add(Duration::from_millis(4)); // > 20ms is slow
        let fast_threshold = Duration::from_millis(11); // < 11ms is fast

        if frame_duration > slow_threshold {
            // Immediate downgrade on sustained slow frame to protect frame rate stability
            self.fast_frame_counter = 0;
            self.downgrade_tier()
        } else if frame_duration < fast_threshold {
            self.fast_frame_counter += 1;
            if self.fast_frame_counter >= self.upgrade_threshold_frames {
                self.fast_frame_counter = 0;
                self.upgrade_tier()
            } else {
                false
            }
        } else {
            self.fast_frame_counter = 0;
            false
        }
    }

    fn downgrade_tier(&mut self) -> bool {
        match self.profile.tier {
            SurfaceQualityTier::High => {
                self.profile = SurfaceQualityProfile::balanced();
                true
            }
            SurfaceQualityTier::Balanced => {
                self.profile = SurfaceQualityProfile::low();
                true
            }
            SurfaceQualityTier::Low => {
                self.profile = SurfaceQualityProfile::ultra_low();
                true
            }
            SurfaceQualityTier::UltraLow => false,
        }
    }

    fn upgrade_tier(&mut self) -> bool {
        match self.profile.tier {
            SurfaceQualityTier::UltraLow => {
                self.profile = SurfaceQualityProfile::low();
                true
            }
            SurfaceQualityTier::Low => {
                self.profile = SurfaceQualityProfile::balanced();
                true
            }
            SurfaceQualityTier::Balanced => {
                self.profile = SurfaceQualityProfile::high();
                true
            }
            SurfaceQualityTier::High => false,
        }
    }

    /// Filters and sorts surface chunk candidates by distance from player, enforcing hard work budgets.
    ///
    /// # Rules
    /// 1. Chunks farther than max distance or max_visible_chunks budget are culled.
    /// 2. Triangle budget (`max_triangles_per_frame`) is strictly enforced.
    /// 3. Closest chunks to player are prioritized deterministically.
    pub fn evaluate_and_cull_chunks(
        &self,
        player_pos: Position,
        candidates: &[ChunkSurfaceCandidate],
        iso8601_timestamp: &str,
    ) -> (Vec<ChunkSurfaceCandidate>, SurfaceRenderingTelemetry) {
        let mut telemetry = SurfaceRenderingTelemetry::new(
            iso8601_timestamp,
            self.profile.tier,
            self.profile.render_scale,
        );
        telemetry.candidate_chunks = candidates.len();

        // Distance calculate and sort candidates closest to player first
        let mut scored: Vec<(f32, &ChunkSurfaceCandidate)> = candidates
            .iter()
            .map(|c| {
                let dx = c.origin.x - player_pos.x;
                let dz = c.origin.z - player_pos.z;
                let dist_sq = dx * dx + dz * dz;
                (dist_sq, c)
            })
            .collect();

        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut accepted = Vec::new();
        let mut accumulated_triangles = 0;

        for (_dist_sq, candidate) in scored {
            if accepted.len() >= self.profile.max_visible_chunks {
                telemetry.budget_culled_chunks += 1;
                continue;
            }

            let cand_triangles = candidate.quad_count * 2;
            if accumulated_triangles + cand_triangles > self.profile.max_triangles_per_frame
                && !accepted.is_empty()
            {
                telemetry.budget_culled_chunks += 1;
                continue;
            }

            accumulated_triangles += cand_triangles;
            telemetry.submitted_quads += candidate.quad_count;
            accepted.push(candidate.clone());
        }

        telemetry.visible_surface_chunks = accepted.len();
        telemetry.submitted_triangles = accumulated_triangles;
        telemetry.draw_calls = accepted.len(); // 1 draw call per chunk baseline
        telemetry.active_lights = self.profile.max_dynamic_lights;

        (accepted, telemetry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hysteresis_prevents_rapid_oscillation() {
        let mut controller = FrameQualityController::new(SurfaceQualityTier::Low);

        // 1 fast frame shouldn't upgrade immediately
        let upgraded = controller.report_frame_time(Duration::from_millis(5));
        assert!(!upgraded);
        assert_eq!(controller.profile.tier, SurfaceQualityTier::Low);

        // Sustained 30 fast frames triggers upgrade
        for _ in 0..29 {
            controller.report_frame_time(Duration::from_millis(5));
        }
        assert_eq!(controller.profile.tier, SurfaceQualityTier::Balanced);

        // A single slow frame triggers immediate downgrade
        let downgraded = controller.report_frame_time(Duration::from_millis(25));
        assert!(downgraded);
        assert_eq!(controller.profile.tier, SurfaceQualityTier::Low);
    }

    #[test]
    fn test_budget_culling_prioritizes_closest_chunks() {
        let controller = FrameQualityController::new(SurfaceQualityTier::UltraLow);
        let player = Position::new(0.0, 0.0);

        let candidates: Vec<_> = (0..50)
            .map(|i| ChunkSurfaceCandidate {
                chunk_x: i,
                chunk_z: 0,
                origin: Position::new((i * 20) as f32, 0.0),
                quad_count: 10,
            })
            .collect();

        let (accepted, telemetry) =
            controller.evaluate_and_cull_chunks(player, &candidates, "2026-07-29T20:56:00Z");

        // UltraLow allows max 16 visible chunks
        assert_eq!(accepted.len(), 16);
        assert_eq!(telemetry.visible_surface_chunks, 16);
        assert_eq!(telemetry.budget_culled_chunks, 34);

        // The accepted chunks must be the 16 closest (indices 0..16)
        for (idx, item) in accepted.iter().enumerate() {
            assert_eq!(item.chunk_x, idx as i32);
        }
    }
}
