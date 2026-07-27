//! The output of one level generator invocation: a pure spatial `VoxelGrid`
//! paired with the non-spatial entities exported by the same generation
//! pass. Kept as a use-case-layer bundle rather than a `VoxelGrid` field so
//! the domain entity stays free of gameplay/infrastructure concerns.

use std::ops::{Deref, DerefMut};

use crate::domain::entities::chunk_entities::ChunkEntities;
use crate::domain::entities::voxel_grid::VoxelGrid;

pub struct GeneratedChunk {
    pub grid: VoxelGrid,
    pub entities: ChunkEntities,
}

impl GeneratedChunk {
    pub fn new(grid: VoxelGrid) -> Self {
        Self {
            grid,
            entities: ChunkEntities::default(),
        }
    }
}

impl Deref for GeneratedChunk {
    type Target = VoxelGrid;

    fn deref(&self) -> &VoxelGrid {
        &self.grid
    }
}

impl DerefMut for GeneratedChunk {
    fn deref_mut(&mut self) -> &mut VoxelGrid {
        &mut self.grid
    }
}
