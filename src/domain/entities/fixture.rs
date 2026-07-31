//! # Ceiling Light Fixture Domain Entities
//!
//! ## Business Domain Rationale & Architectural Intent
//! In architectural space planning for procedural infinite structures, physical light
//! fixtures (such as recessed fluorescent troffers, strip lights, and emergency lights)
//! represent distinct physical objects attached to ceiling surfaces.
//!
//! Historically in grid-based procedural engines, light sampling evaluated whether a point
//! in space was "lit" by re-evaluating periodic modulo functions at each point. This led to
//! spatial artifacts where panel center coordinates shifted with query positions, and fixture
//! identity changed across chunk boundaries.
//!
//! To uphold Clean Architecture and domain integrity, this module defines the explicit
//! Domain Entities representing light fixtures:
//! - [`FixtureId`]: A stable 64-bit canonical identifier unique to a physical fixture.
//! - [`FixtureKind`]: The physical form factor and fixture class (Panel, Strip, Emergency).
//! - [`FixtureState`]: The physical operational state (Lit, Flickering, Dim, Dead, Emergency).
//! - [`FixtureLayout`]: The complete physical entity bounded in world space (center, extents, height).
//! - [`FixtureSample`]: A Flyweight point-query view stored within spatial column plans.
//!
//! ## Design Pattern Strategy: Flyweight Pattern
//! The [`FixtureSample`] struct serves as a **Flyweight Pattern** instance. Rather than allocating
//! heap-bound objects for millions of voxel columns in streamed chunk windows, column plans store
//! a lightweight, copyable sample referencing the canonical geometry and state of the parent
//! [`FixtureLayout`]. This ensures O(1) performance while guaranteeing that every point query within
//! a fixture's footprint returns identical, deterministic metadata (including fixed panel center coordinates).

use std::fmt::{Display, Formatter};

/// A 64-bit unique identifier representing a single logical ceiling fixture entity.
/// Must remain stable regardless of chunk boundaries, generation order, or LOD level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FixtureId(pub u64);

impl Display for FixtureId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "FixtureId(0x{:016X})", self.0)
    }
}

/// The physical classification and geometry profile of a ceiling light fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixtureKind {
    /// Standard square fluorescent ceiling troffer (e.g. 0.9m x 0.9m).
    FluorescentPanel,
    /// Continuous or elongated fluorescent strip light (e.g. 1.2m x 0.6m).
    FluorescentStrip,
    /// Low-intensity emergency LED/incandescent strip active during blackout conditions.
    EmergencyStrip,
    /// Physical fixture panel whose electrical source has failed, rendering it non-emissive.
    DeadPanel,
}

/// Operational electrical condition of a fixture entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixtureState {
    /// Fully powered and emitting nominal light output.
    Lit,
    /// Intermittently flickering due to power fluctuation or ballast aging.
    Flickering,
    /// Emitting reduced light output due to undervoltage or tube degradation.
    Dim,
    /// Unpowered or burned out; physically present but non-emissive.
    Dead,
    /// Low-power emergency illumination active during primary power failure.
    Emergency,
}

/// Canonical Domain Entity representing a physical light fixture attached to a ceiling surface.
///
/// Contains the stable identity, bounding extents in world space, ceiling height, and state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixtureLayout {
    /// Unique stable 64-bit identifier for this fixture entity.
    pub id: u64,
    /// Physical fixture classification.
    pub kind: FixtureKind,
    /// Operational state governing emission and material assignment.
    pub state: FixtureState,
    /// Center position X coordinate in world space (meters).
    pub center_x: f32,
    /// Center position Z coordinate in world space (meters).
    pub center_z: f32,
    /// Half-extent width along the X axis in world space (meters).
    pub half_x: f32,
    /// Half-extent depth along the Z axis in world space (meters).
    pub half_z: f32,
    /// Absolute ceiling plane height above world floor (meters).
    pub ceiling_units: f32,
    /// True when this fixture belongs to a Red Room anomaly (emits red pressure light).
    pub red_room: bool,
}

impl FixtureLayout {
    /// Creates a new canonical [`FixtureLayout`] entity.
    pub fn new(
        id: u64,
        kind: FixtureKind,
        state: FixtureState,
        center_x: f32,
        center_z: f32,
        half_x: f32,
        half_z: f32,
        ceiling_units: f32,
        red_room: bool,
    ) -> Self {
        Self {
            id,
            kind,
            state,
            center_x,
            center_z,
            half_x,
            half_z,
            ceiling_units,
            red_room,
        }
    }

    /// Pure function returning whether a world-space point `(wx, wz)` lies strictly inside
    /// this fixture's rectangular ceiling footprint.
    pub fn contains_point(&self, wx: f32, wz: f32) -> bool {
        (wx - self.center_x).abs() <= self.half_x && (wz - self.center_z).abs() <= self.half_z
    }

    /// Converts this canonical entity into a lightweight point-query [`FixtureSample`].
    pub fn to_sample(&self) -> FixtureSample {
        FixtureSample {
            id: self.id,
            kind: self.kind,
            state: self.state,
            center_x: self.center_x,
            center_z: self.center_z,
            half_x: self.half_x,
            half_z: self.half_z,
            ceiling_units: self.ceiling_units,
            red_room: self.red_room,
        }
    }

    /// Returns true if the fixture state is actively emitting light.
    pub fn is_emissive(&self) -> bool {
        matches!(
            self.state,
            FixtureState::Lit
                | FixtureState::Flickering
                | FixtureState::Dim
                | FixtureState::Emergency
        )
    }
}

/// Point-query sample representation of a light fixture, carried within spatial column plans.
///
/// Implements the Flyweight pattern: carries exact stable center coordinates and fixture identity
/// so that point queries across a fixture's footprint remain deterministic and identity-preserving.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixtureSample {
    /// Unique stable 64-bit identifier for the parent fixture entity.
    pub id: u64,
    /// Physical fixture classification.
    pub kind: FixtureKind,
    /// Operational state governing emission and material assignment.
    pub state: FixtureState,
    /// Stable center position X coordinate in world space (meters).
    pub center_x: f32,
    /// Stable center position Z coordinate in world space (meters).
    pub center_z: f32,
    /// Half-extent width along X axis (meters).
    pub half_x: f32,
    /// Half-extent depth along Z axis (meters).
    pub half_z: f32,
    /// Absolute ceiling plane height (meters).
    pub ceiling_units: f32,
    /// True when belonging to a Red Room anomaly.
    pub red_room: bool,
}

impl FixtureSample {
    /// Pure function checking if a world-space point `(wx, wz)` falls within this sample's footprint.
    pub fn contains_point(&self, wx: f32, wz: f32) -> bool {
        (wx - self.center_x).abs() <= self.half_x && (wz - self.center_z).abs() <= self.half_z
    }

    /// Returns true if this sampled fixture is actively emitting light.
    pub fn is_emissive(&self) -> bool {
        matches!(
            self.state,
            FixtureState::Lit
                | FixtureState::Flickering
                | FixtureState::Dim
                | FixtureState::Emergency
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_layout_contains_point_and_to_sample_are_deterministic() {
        let layout = FixtureLayout::new(
            0x1234_5678_9ABC_DEF0,
            FixtureKind::FluorescentPanel,
            FixtureState::Lit,
            10.0,
            20.0,
            0.5,
            0.5,
            3.6,
            false,
        );

        assert!(layout.contains_point(10.0, 20.0));
        assert!(layout.contains_point(10.4, 20.4));
        assert!(!layout.contains_point(10.6, 20.0));
        assert!(!layout.contains_point(10.0, 20.6));

        let sample = layout.to_sample();
        assert_eq!(sample.id, 0x1234_5678_9ABC_DEF0);
        assert_eq!(sample.center_x, 10.0);
        assert_eq!(sample.center_z, 20.0);
        assert!(sample.is_emissive());
    }
}
