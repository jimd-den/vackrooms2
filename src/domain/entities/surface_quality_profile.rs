//! # Surface Quality Profile Entity
//!
//! ## Business Intent & Rationale
//! In infinite procedural voxel architectures (Vackrooms), hardware capabilities vary across
//! a vast spectrum—from low-power mobile devices and WebGL browsers to high-end WebGPU rigs.
//! This module encapsulates enterprise rules defining quality tiers and work budgets for
//! surface rendering.
//!
//! Low-spec surface rendering prioritizes stable frame pacing over per-voxel lighting detail.
//! Continuous architectural elements (floors, walls, ceilings) are greedily merged into minimal
//! 2-triangle quads, and lighting is represented via low-frequency 3D probe volumes rather than
//! expensive per-fragment scene traversal or high-density light maps.
//!
//! ## Design Patterns Used
//! - **Factory Pattern**: Static factory constructors (`ultra_low`, `low`, `balanced`, `high`)
//!   create pre-configured, validated quality profiles tailored to hardware capabilities.
//! - **Strategy Pattern Integration**: `SurfaceQualityProfile` couples tier definitions with
//!   the appropriate `SurfaceMeshingPolicy` strategy (`LowSpec` vs `Exact`).

use crate::adapters::voxel_mapper::SurfaceMeshingPolicy;

/// Quality tier classifications for surface rendering performance governance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceQualityTier {
    UltraLow,
    #[default]
    Low,
    Balanced,
    High,
}

/// Enterprise entity defining surface quality parameters, resolution scaling, and GPU work budgets.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceQualityProfile {
    /// The canonical quality tier.
    pub tier: SurfaceQualityTier,
    /// The greedy meshing policy to use (Exact vs LowSpec).
    pub meshing_policy: SurfaceMeshingPolicy,
    /// Internal render resolution scale multiplier (clamped to [0.25, 1.0]).
    pub render_scale: f32,
    /// Maximum number of active chunks visible per frame.
    pub max_visible_chunks: usize,
    /// Maximum active dynamic light sources.
    pub max_dynamic_lights: usize,
    /// Whether dynamic shadow rendering is enabled.
    pub enable_shadows: bool,
    /// Spatial resolution of the low-frequency 3D probe lighting volume (width, height, depth).
    pub probe_resolution: (usize, usize, usize),
    /// Maximum number of surface triangles allowed in a single frame.
    pub max_triangles_per_frame: usize,
}

impl SurfaceQualityProfile {
    /// Constructs an UltraLow quality profile targeted at weak integrated GPUs and mobile web contexts.
    pub fn ultra_low() -> Self {
        Self {
            tier: SurfaceQualityTier::UltraLow,
            meshing_policy: SurfaceMeshingPolicy::LowSpec,
            render_scale: 0.5,
            max_visible_chunks: 16,
            max_dynamic_lights: 0,
            enable_shadows: false,
            probe_resolution: (4, 2, 4),
            max_triangles_per_frame: 50_000,
        }
    }

    /// Constructs a Low quality profile targeted at baseline low-spec desktop hardware.
    pub fn low() -> Self {
        Self {
            tier: SurfaceQualityTier::Low,
            meshing_policy: SurfaceMeshingPolicy::LowSpec,
            render_scale: 0.75,
            max_visible_chunks: 36,
            max_dynamic_lights: 1,
            enable_shadows: false,
            probe_resolution: (8, 4, 8),
            max_triangles_per_frame: 150_000,
        }
    }

    /// Constructs a Balanced quality profile for standard desktop GPUs.
    pub fn balanced() -> Self {
        Self {
            tier: SurfaceQualityTier::Balanced,
            meshing_policy: SurfaceMeshingPolicy::Exact,
            render_scale: 1.0,
            max_visible_chunks: 64,
            max_dynamic_lights: 4,
            enable_shadows: true,
            probe_resolution: (8, 4, 8),
            max_triangles_per_frame: 500_000,
        }
    }

    /// Constructs a High quality profile for high-end GPUs.
    pub fn high() -> Self {
        Self {
            tier: SurfaceQualityTier::High,
            meshing_policy: SurfaceMeshingPolicy::Exact,
            render_scale: 1.0,
            max_visible_chunks: 121,
            max_dynamic_lights: 8,
            enable_shadows: true,
            probe_resolution: (16, 8, 16),
            max_triangles_per_frame: 1_500_000,
        }
    }

    /// Validates and clamps numerical boundaries to safe operational ranges.
    pub fn sanitized(&self) -> Self {
        let mut copy = self.clone();
        copy.render_scale = copy.render_scale.clamp(0.25, 1.0);
        copy.max_visible_chunks = copy.max_visible_chunks.max(1);
        copy.probe_resolution.0 = copy.probe_resolution.0.max(1);
        copy.probe_resolution.1 = copy.probe_resolution.1.max(1);
        copy.probe_resolution.2 = copy.probe_resolution.2.max(1);
        copy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quality_profile_tier_defaults() {
        let ultra = SurfaceQualityProfile::ultra_low();
        assert_eq!(ultra.meshing_policy, SurfaceMeshingPolicy::LowSpec);
        assert!(!ultra.enable_shadows);
        assert_eq!(ultra.max_dynamic_lights, 0);

        let high = SurfaceQualityProfile::high();
        assert_eq!(high.meshing_policy, SurfaceMeshingPolicy::Exact);
        assert!(high.enable_shadows);
        assert!(high.max_dynamic_lights > 0);
    }

    #[test]
    fn test_sanitization_bounds() {
        let mut custom = SurfaceQualityProfile::ultra_low();
        custom.render_scale = 0.05; // Below legal bound
        custom.max_visible_chunks = 0;
        let clean = custom.sanitized();
        assert_eq!(clean.render_scale, 0.25);
        assert_eq!(clean.max_visible_chunks, 1);
    }
}
