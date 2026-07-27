//! Non-spatial gameplay/state artifacts a level generator exports alongside
//! a chunk's voxel volume. Kept separate from `VoxelGrid` so the volume
//! stays a pure spatial data structure: these vectors are typically empty or
//! a handful of entries, decided by the same immutable plans as the
//! geometry, not derived from the voxels themselves.

use crate::domain::entities::anomaly::{LevelExit, PitHazard, TraversalGate};
use crate::domain::entities::architecture::RuntimeLight;
use crate::domain::entities::supplies::SupplyItem;

#[derive(Default, Clone)]
pub struct ChunkEntities {
    pub runtime_lights: Vec<RuntimeLight>,
    pub traversal_gates: Vec<TraversalGate>,
    pub pit_hazards: Vec<PitHazard>,
    pub supply_items: Vec<SupplyItem>,
    pub level_exits: Vec<LevelExit>,
}
