//! Cellular-automata environmental decay: dampness creeps, wallpaper ages,
//! aged walls crumble.
//!
//! One step is a pure, synchronous transition over the horizontal Moore
//! neighborhood (the eight same-height neighbors): every decision reads the
//! *previous* state only, so stepping is order-independent and
//! deterministic. Decay is a post-pass an integrator applies a fixed number
//! of times after voxelization — never during sampling, where neighbor
//! reads would break the chunk-independence contract.

use crate::domain::entities::voxel_grid::{
    VOXEL_AGED_WALLPAPER, VOXEL_AIR, VOXEL_DAMAGED_WALL, VOXEL_DRY_CARPET, VOXEL_FLUID,
    VOXEL_STICKY_CARPET, VOXEL_WALL, VOXEL_WATER, VoxelGrid,
};

/// Thresholds for the three transition rules, in damp-neighbor counts out
/// of the eight horizontal neighbors.
#[derive(Debug, Clone, Copy)]
pub struct DecayRules {
    /// Dry carpet saturates into sticky carpet at this many damp neighbors.
    pub carpet_saturates_at: u8,
    /// Exposed wall (>=1 air neighbor) grows aged wallpaper at this many.
    pub wall_ages_at: u8,
    /// Aged wallpaper crumbles into damaged wall at this many.
    pub wallpaper_crumbles_at: u8,
}

impl Default for DecayRules {
    fn default() -> Self {
        Self {
            carpet_saturates_at: 3,
            wall_ages_at: 2,
            wallpaper_crumbles_at: 4,
        }
    }
}

/// Materials that radiate dampness onto their neighbors.
const DAMP_SOURCES: [u8; 3] = [VOXEL_STICKY_CARPET, VOXEL_FLUID, VOXEL_WATER];

/// The horizontal Moore neighborhood: neighbor damp/air census at (x, y, z).
fn census(grid: &VoxelGrid, x: usize, y: usize, z: usize) -> (u8, u8) {
    let (mut damp, mut air) = (0u8, 0u8);
    for dz in -1i64..=1 {
        for dx in -1i64..=1 {
            if dx == 0 && dz == 0 {
                continue;
            }
            let (nx, nz) = (x as i64 + dx, z as i64 + dz);
            if nx < 0 || nz < 0 || nx >= grid.width() as i64 || nz >= grid.depth() as i64 {
                continue;
            }
            let material = grid.get(nx as usize, y, nz as usize);
            if DAMP_SOURCES.contains(&material) {
                damp += 1;
            } else if material == VOXEL_AIR {
                air += 1;
            }
        }
    }
    (damp, air)
}

/// The next state of one cell, or `None` when it keeps its material.
fn transition(rules: &DecayRules, current: u8, damp: u8, air: u8) -> Option<u8> {
    match current {
        VOXEL_DRY_CARPET if damp >= rules.carpet_saturates_at => Some(VOXEL_STICKY_CARPET),
        VOXEL_WALL if air >= 1 && damp >= rules.wall_ages_at => Some(VOXEL_AGED_WALLPAPER),
        VOXEL_AGED_WALLPAPER if damp >= rules.wallpaper_crumbles_at => Some(VOXEL_DAMAGED_WALL),
        _ => None,
    }
}

/// Advances the automaton one generation in place. Returns how many cells
/// changed, so integrators can run to quiescence (`while step(..) > 0`).
pub fn step_cellular_decay(grid: &mut VoxelGrid, rules: &DecayRules) -> usize {
    // Read the whole previous generation before the first write.
    let mut changes = Vec::new();
    for y in 0..grid.height() {
        for z in 0..grid.depth() {
            for x in 0..grid.width() {
                let current = grid.get(x, y, z);
                let (damp, air) = census(grid, x, y, z);
                if let Some(next) = transition(rules, current, damp, air) {
                    changes.push((x, y, z, next));
                }
            }
        }
    }
    for &(x, y, z, material) in &changes {
        grid.set(x, y, z, material);
    }
    changes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn damp_ring(grid: &mut VoxelGrid, x: usize, z: usize, count: usize) {
        let offsets = [
            (-1i64, -1i64),
            (0, -1),
            (1, -1),
            (-1, 0),
            (1, 0),
            (-1, 1),
            (0, 1),
            (1, 1),
        ];
        for &(dx, dz) in offsets.iter().take(count) {
            grid.set(
                (x as i64 + dx) as usize,
                0,
                (z as i64 + dz) as usize,
                VOXEL_FLUID,
            );
        }
    }

    #[test]
    fn surrounded_dry_carpet_saturates_and_isolated_carpet_stays_dry() {
        let mut grid = VoxelGrid::new(8, 1, 8);
        grid.set(2, 0, 2, VOXEL_DRY_CARPET);
        damp_ring(&mut grid, 2, 2, 3);
        grid.set(6, 0, 6, VOXEL_DRY_CARPET);

        let changed = step_cellular_decay(&mut grid, &DecayRules::default());
        assert_eq!(changed, 1);
        assert_eq!(grid.get(2, 0, 2), VOXEL_STICKY_CARPET);
        assert_eq!(grid.get(6, 0, 6), VOXEL_DRY_CARPET);
    }

    #[test]
    fn only_air_exposed_walls_grow_aged_wallpaper() {
        let mut grid = VoxelGrid::new(8, 1, 8);
        // Exposed wall: two damp neighbors, the rest air.
        grid.set(2, 0, 2, VOXEL_WALL);
        damp_ring(&mut grid, 2, 2, 2);
        // Buried wall: same dampness but every remaining neighbor is wall.
        grid.set(6, 0, 6, VOXEL_WALL);
        damp_ring(&mut grid, 6, 6, 2);
        for (dx, dz) in [(1i64, -1i64), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
            grid.set((6 + dx) as usize, 0, (6 + dz) as usize, VOXEL_WALL);
        }

        step_cellular_decay(&mut grid, &DecayRules::default());
        assert_eq!(grid.get(2, 0, 2), VOXEL_AGED_WALLPAPER);
        assert_eq!(grid.get(6, 0, 6), VOXEL_WALL, "buried wall must not age");
    }

    #[test]
    fn deeply_damp_wallpaper_crumbles() {
        let mut grid = VoxelGrid::new(8, 1, 8);
        grid.set(3, 0, 3, VOXEL_AGED_WALLPAPER);
        damp_ring(&mut grid, 3, 3, 4);
        step_cellular_decay(&mut grid, &DecayRules::default());
        assert_eq!(grid.get(3, 0, 3), VOXEL_DAMAGED_WALL);
    }

    #[test]
    fn transitions_read_the_previous_generation_only() {
        // A carpet chain where cell B saturates this step: cell C counts
        // B's *old* dry state, so C must not saturate in the same step.
        let mut grid = VoxelGrid::new(10, 1, 10);
        grid.set(4, 0, 4, VOXEL_DRY_CARPET);
        damp_ring(&mut grid, 4, 4, 3);
        grid.set(5, 0, 5, VOXEL_DRY_CARPET);
        // (5,5) sees exactly the two fluid cells of B's ring that touch it
        // diagonally, plus B itself — dry this generation.
        let changed = step_cellular_decay(&mut grid, &DecayRules::default());
        assert_eq!(changed, 1, "only B changes in generation one");
        assert_eq!(grid.get(5, 0, 5), VOXEL_DRY_CARPET);
    }

    #[test]
    fn quiet_grids_are_a_fixpoint() {
        let mut grid = VoxelGrid::new(6, 2, 6);
        grid.set(1, 0, 1, VOXEL_WALL);
        grid.set(2, 0, 2, VOXEL_DRY_CARPET);
        assert_eq!(step_cellular_decay(&mut grid, &DecayRules::default()), 0);
    }
}
