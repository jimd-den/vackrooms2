//! # Fixture Electrical Maintenance & Failure Simulation
//!
//! ## Domain Rationale & Architectural Intent
//! In real commercial and institutional infrastructure, fixture degradation and electrical failures
//! do not occur as independent per-panel random flips. Power circuits, transformer branches, and
//! ballast wear cluster spatially by electrical district, room enclosure, or corridor segment.
//!
//! This module decouples **fixture placement** (derived strictly from architecture) from
//! **fixture maintenance state** (derived from spatially coherent electrical districts).
//!
//! ## Design Pattern Strategy: Strategy Pattern & Pure Functions
//! The maintenance layer implements a **Strategy Pattern** for environmental decay:
//! 1. [`ElectricalCondition`]: Evaluates the broad electrical health of a spatial district
//!    (Normal, Degraded, Failing, Dead) based on world seed, district identity, and institutional age.
//! 2. [`fixture_state_for`]: Maps an [`ElectricalCondition`] and [`FixtureKind`] into a concrete
//!    operational [`FixtureState`] (Lit, Flickering, Dim, Dead, Emergency).
//!
//! This design ensures deterministic spatial clustering: an entire corridor section or office suite
//! shares a common circuit condition, while individual fixtures exhibit minor variance. Emergency
//! fixtures selectively survive even when primary power is Dead.

use crate::domain::entities::fixture::{FixtureKind, FixtureState};

/// The operational electrical health of a spatially coherent circuit or district.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ElectricalCondition {
    /// Nominal power supply; fixtures operate normally.
    Normal,
    /// Low voltage / old ballast; fixtures operate in a dim state.
    Degraded,
    /// Intermittent power supply; fixtures exhibit flickering behavior.
    Failing,
    /// Complete circuit failure; primary lights are dead.
    Dead,
}

/// Pure hashing function computing deterministic unit float `[0.0, 1.0)` for maintenance decisions.
fn maintenance_hash(seed: u32, salt: u64, id: u64) -> f32 {
    let mut h = (seed as u64)
        ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ id.rotate_left(23);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// Evaluates the spatially coherent [`ElectricalCondition`] for an electrical district or segment.
///
/// Pure function of world seed, district identifier, and institutional age parameter.
pub(crate) fn electrical_condition_for_district(
    seed: u32,
    district_id: u64,
    institution_age: f32,
) -> ElectricalCondition {
    let roll = maintenance_hash(seed, 0xE1EC_7F10, district_id);

    // Aging shifts the probability distribution toward degraded/failing/dead conditions
    let dead_threshold = (0.05 + 0.25 * institution_age).min(0.50);
    let failing_threshold = dead_threshold + (0.05 + 0.15 * institution_age).min(0.30);
    let degraded_threshold = failing_threshold + (0.10 + 0.20 * institution_age).min(0.30);

    if roll < dead_threshold {
        ElectricalCondition::Dead
    } else if roll < failing_threshold {
        ElectricalCondition::Failing
    } else if roll < degraded_threshold {
        ElectricalCondition::Degraded
    } else {
        ElectricalCondition::Normal
    }
}

/// Maps an [`ElectricalCondition`] and [`FixtureKind`] to a concrete operational [`FixtureState`].
///
/// Incorporates a per-fixture variance hash so that panels within a failing circuit do not all
/// flicker on the exact same frame phase.
pub(crate) fn fixture_maintenance_state(
    condition: ElectricalCondition,
    kind: FixtureKind,
    fixture_hash_val: f32,
) -> FixtureState {
    match condition {
        ElectricalCondition::Normal => {
            if fixture_hash_val < 0.02 {
                FixtureState::Flickering
            } else if fixture_hash_val < 0.05 {
                FixtureState::Dim
            } else {
                FixtureState::Lit
            }
        }
        ElectricalCondition::Degraded => {
            if fixture_hash_val < 0.20 {
                FixtureState::Flickering
            } else {
                FixtureState::Dim
            }
        }
        ElectricalCondition::Failing => {
            if fixture_hash_val < 0.70 {
                FixtureState::Flickering
            } else {
                FixtureState::Dim
            }
        }
        ElectricalCondition::Dead => {
            if matches!(kind, FixtureKind::EmergencyStrip) || fixture_hash_val < 0.05 {
                FixtureState::Emergency
            } else {
                FixtureState::Dead
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_state_is_coherent_and_deterministic() {
        let cond_normal = electrical_condition_for_district(42, 100, 0.0);
        let cond_aged = electrical_condition_for_district(42, 100, 1.0);

        // Same inputs produce identical results
        assert_eq!(cond_normal, electrical_condition_for_district(42, 100, 0.0));
        assert_eq!(cond_aged, electrical_condition_for_district(42, 100, 1.0));

        let state_lit = fixture_maintenance_state(ElectricalCondition::Normal, FixtureKind::FluorescentPanel, 0.5);
        assert_eq!(state_lit, FixtureState::Lit);

        let state_emergency = fixture_maintenance_state(ElectricalCondition::Dead, FixtureKind::EmergencyStrip, 0.5);
        assert_eq!(state_emergency, FixtureState::Emergency);
    }
}
