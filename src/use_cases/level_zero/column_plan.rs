//! # Column Plan Vocabulary
//!
//! ## Architectural Rationale
//! Represents the 2.5D architectural profile of a single vertical column in Level 0 space.
//! Integrates floor height, solid mass, ceiling plane height, lintels, doors, wall/floor materials,
//! and ceiling light fixture samples ([`FixtureSample`]).

use crate::domain::entities::voxel_grid::{VOXEL_FLOOR, VOXEL_LIGHT, VOXEL_WALL};

pub(crate) use crate::domain::entities::fixture::{FixtureKind, FixtureSample, FixtureState};

/// What one (x, z) column of the level looks like.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ColumnPlan {
    /// Whether the walkable floor slab exists under this column.
    pub floor: bool,
    /// Raised solid floor above the base slab, world units.
    pub floor_units: f32,
    /// Floor-to-ceiling solid (wall or pillar).
    pub solid: bool,
    /// Ceiling height in world units (top of the walkable space).
    pub ceiling_units: f32,
    /// A light fixture occupies this ceiling rectangle, if one is planned
    /// for this column's position.
    pub fixture: Option<FixtureSample>,
    /// Solid band hanging from the ceiling down to this height (door lintels).
    pub lintel_from_units: Option<f32>,
    /// A visible, non-colliding door leaf occupies the opening below its lintel.
    pub door_leaf: bool,
    /// Material used for solid columns and lintels.
    pub wall_material: u8,
    /// Material of the floor slab.
    pub floor_material: u8,
    /// Material of the fixture voxel when the fixture is lit.
    pub light_material: u8,
}

impl ColumnPlan {
    /// An open ordinary-fabric column with the default Level 0 materials.
    pub(crate) fn open(ceiling_units: f32) -> Self {
        Self {
            floor: true,
            floor_units: 0.0,
            solid: false,
            ceiling_units,
            fixture: None,
            lintel_from_units: None,
            door_leaf: false,
            wall_material: VOXEL_WALL,
            floor_material: VOXEL_FLOOR,
            light_material: VOXEL_LIGHT,
        }
    }

    /// Does this column carry a fixture that is currently emitting light?
    pub(crate) fn has_lit_fixture(self) -> bool {
        self.fixture.map_or(false, |f| f.is_emissive())
    }
}
