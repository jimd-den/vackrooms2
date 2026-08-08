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
    /// The fit-out layers this column carries above bare structure.
    pub assembly: AssemblyStack,
}

/// The construction layers of one column, over and above "solid or not".
///
/// A wall in a real building is an assembly — base, core, finish — and a
/// dropped ceiling is a grid carrying tiles with a plenum above. Modelling
/// a wall as one material floor-to-ceiling is what makes voxel architecture
/// read as terrain: real walls have a visible bottom edge, and real ceilings
/// have a module. This is the per-column slice of that assembly, the
/// voxel equivalent of an `IfcMaterialLayerSet`.
///
/// Kept as plain `Copy` data on `ColumnPlan` so sampling stays a pure
/// function of position with no allocation — the same contract the rest of
/// the column vocabulary keeps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AssemblyStack {
    /// Height of the base course at the foot of a solid column, world
    /// units. Zero leaves the wall bare, which is correct for raw shells
    /// (Level 1) and for anything that was never fitted out.
    pub baseboard_units: f32,
    /// This ceiling column is a grid runner rather than a tile.
    pub ceiling_grid: bool,
    /// The tile here is gone: the ceiling plane opens and the plenum above
    /// becomes visible. The signature Backrooms ceiling motif, and the
    /// architectural seam to Level 1.
    pub tile_missing: bool,
    /// Depth of the plenum above the finished ceiling, world units. Zero
    /// means this column has no room for one — a vault ceiling can reach
    /// the slab, and then there is nothing to hide.
    pub plenum_units: f32,
    /// What occupies the plenum at this column, seen through a missing
    /// tile. `None` is empty plenum air.
    pub plenum_content: Option<u8>,
}

impl AssemblyStack {
    /// Bare structure: no base, no ceiling system, no plenum. The correct
    /// default for fabric and for anything not deliberately fitted out.
    pub(crate) const fn bare() -> Self {
        Self {
            baseboard_units: 0.0,
            ceiling_grid: false,
            tile_missing: false,
            plenum_units: 0.0,
            plenum_content: None,
        }
    }
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
            assembly: AssemblyStack::bare(),
        }
    }

    /// Does this column carry a fixture that is currently emitting light?
    pub(crate) fn has_lit_fixture(self) -> bool {
        self.fixture.map_or(false, |f| f.is_emissive())
    }
}
