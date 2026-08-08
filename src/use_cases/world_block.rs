//! Loading a place instead of streaming a frontier.
//!
//! The level used to plan chunk-locally: every architectural decision had to
//! be answerable from one coordinate with no cross-chunk reads, because a
//! chunk arriving in front of the player had nobody to ask. That constraint
//! bought infinite streaming and cost the architecture nearly everything
//! that needs to see more than one room — connectivity you can *verify*
//! rather than assume, integration over a circulation graph, composition
//! staged across an area, a motif established before it is broken.
//!
//! A [`WorldBlock`] is the trade taken deliberately: behind one loading
//! screen, plan a very large area at once. Only the *plans* are bulk-built —
//! a region is a few kilobytes of assemblies, corridors and genomes, so a
//! whole block is a few hundred kilobytes. Voxels keep streaming from those
//! plans exactly as before, which is what keeps frame time flat on a small
//! machine. Bulk-generating voxels instead would be hundreds of megabytes
//! and would buy nothing: voxel sampling was never the part that was
//! starved for context.
//!
//! Crossing a block is not a level transition. The player walks out of one
//! and into the next with no seam and no pause, because the next was
//! planned in the background the moment they crossed the [`PRELOAD_MARGIN`]
//! — the loading screen is paid once, at the start, and never again.

use crate::domain::entities::anomaly::WorldBounds;
use crate::domain::entities::architecture::RegionPlan;
use crate::domain::entities::position::Position;
use crate::use_cases::generate_chunk::GeneratorConfig;
use crate::use_cases::infinite_level::InfiniteRegionWindow;
use crate::use_cases::ports::NoiseProvider;
use crate::use_cases::region_plan::REGION_SIZE;

/// Regions along one edge of a block.
///
/// Sixteen 80 u regions is a 1280 u square — 1.6 km² of floor. Crossing it
/// in a straight line takes minutes; nobody crosses a labyrinth in a
/// straight line, and finding your way through one is hours. That is the
/// unit the loading screen is buying.
pub const REGIONS_PER_BLOCK: i64 = 16;

/// Side of a block, world units.
pub const BLOCK_SIZE: f32 = REGIONS_PER_BLOCK as f32 * REGION_SIZE;

/// How close to the edge a player must come before the next block starts
/// planning, world units.
///
/// Three regions. Wide enough that even a sprint across the margin finishes
/// planning before the player arrives, narrow enough that a wanderer who
/// merely brushes a boundary and turns back does not trigger work for a
/// place they never visit.
pub const PRELOAD_MARGIN: f32 = 3.0 * REGION_SIZE;

/// Address of a block on the block lattice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockCoord {
    pub x: i64,
    pub z: i64,
}

impl BlockCoord {
    /// The block containing a world position.
    pub fn of(world: Position) -> Self {
        Self {
            x: (world.x / BLOCK_SIZE).floor() as i64,
            z: (world.z / BLOCK_SIZE).floor() as i64,
        }
    }

    pub fn origin(self) -> Position {
        Position::new(self.x as f32 * BLOCK_SIZE, self.z as f32 * BLOCK_SIZE)
    }

    pub fn bounds(self) -> WorldBounds {
        let origin = self.origin();
        WorldBounds::new(
            origin.x,
            origin.z,
            origin.x + BLOCK_SIZE,
            origin.z + BLOCK_SIZE,
        )
    }
}

/// A large, fully planned, walkable area.
pub struct WorldBlock {
    coord: BlockCoord,
    window: InfiniteRegionWindow,
}

impl WorldBlock {
    /// Plans an entire block, reporting progress so a loading screen has
    /// something honest to show.
    ///
    /// `progress` receives `(regions_done, regions_total)`. It is called per
    /// region rather than per chunk because a region is the unit of work
    /// that actually takes time, and a progress bar that advances in units
    /// nobody is waiting on is a lie.
    pub fn load(
        seed: u32,
        coord: BlockCoord,
        config: &GeneratorConfig,
        noise: &dyn NoiseProvider,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Self {
        // One halo region beyond the block, so a chunk sampled at the very
        // edge still finds the plan of the region across the boundary and
        // the two blocks agree about what is built there.
        //
        // The halo is why the reported total is larger than
        // `REGIONS_PER_BLOCK^2`: the window really does plan the surrounding
        // ring, and a bar that hid that work would stall at 100% while it
        // finished.
        let window = InfiniteRegionWindow::covering_with_progress(
            coord.bounds(),
            REGION_SIZE,
            seed,
            config,
            noise,
            progress,
        );
        Self { coord, window }
    }

    /// The planned regions, for feeding straight into chunk generation.
    ///
    /// Handing the window over rather than copying plans out is the whole
    /// economy of the block: one derivation serves every chunk inside it,
    /// and any block-scale pass run over these plans is automatically seen
    /// by all of them.
    pub fn plans(&self) -> &InfiniteRegionWindow {
        &self.window
    }

    pub fn coord(&self) -> BlockCoord {
        self.coord
    }

    pub fn bounds(&self) -> WorldBounds {
        self.coord.bounds()
    }

    /// The plan for a world point, if this block covers it.
    pub fn plan_at(&self, world: Position) -> Option<&RegionPlan> {
        self.window.plan_at(world)
    }

    /// Is this position inside the block proper (not merely its halo)?
    pub fn contains(&self, world: Position) -> bool {
        self.bounds().contains(world.x, world.z)
    }

    /// Blocks that should start planning now, given where the player is.
    ///
    /// Returns the neighbours whose edge the player has come within
    /// [`PRELOAD_MARGIN`] of — up to three at a corner. Empty in the
    /// interior, which is almost always, so the caller does no work for most
    /// of a session.
    ///
    /// Deliberately returns *what to load* rather than loading it: this
    /// module plans, and only the caller knows whether it has a thread to
    /// plan on.
    pub fn preload_targets(&self, world: Position) -> Vec<BlockCoord> {
        let b = self.bounds();
        let mut out = Vec::new();
        let near_west = world.x - b.min_x < PRELOAD_MARGIN;
        let near_east = b.max_x - world.x < PRELOAD_MARGIN;
        let near_north = world.z - b.min_z < PRELOAD_MARGIN;
        let near_south = b.max_z - world.z < PRELOAD_MARGIN;

        let mut push = |dx: i64, dz: i64| {
            out.push(BlockCoord {
                x: self.coord.x + dx,
                z: self.coord.z + dz,
            })
        };
        if near_west {
            push(-1, 0);
        }
        if near_east {
            push(1, 0);
        }
        if near_north {
            push(0, -1);
        }
        if near_south {
            push(0, 1);
        }
        // The diagonal too: a player heading for a corner arrives in the
        // diagonal block without ever entering either edge neighbour, and
        // finding it unplanned is exactly the stall this exists to avoid.
        if near_west && near_north {
            push(-1, -1);
        }
        if near_east && near_north {
            push(1, -1);
        }
        if near_west && near_south {
            push(-1, 1);
        }
        if near_east && near_south {
            push(1, 1);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frameworks_drivers::simple_noise::SimpleNoiseProvider;

    fn config() -> GeneratorConfig {
        GeneratorConfig::low_spec()
    }

    #[test]
    fn a_block_is_a_large_walkable_area() {
        // The number that justifies the loading screen.
        assert!(
            BLOCK_SIZE >= 1000.0,
            "a block should be a place, not a room: {BLOCK_SIZE} u"
        );
    }

    #[test]
    fn block_addressing_round_trips_including_negatives() {
        for (x, z) in [(0.0, 0.0), (1500.0, 20.0), (-10.0, -2000.0), (-0.5, -0.5)] {
            let world = Position::new(x, z);
            let coord = BlockCoord::of(world);
            assert!(
                coord.bounds().contains(world.x, world.z),
                "({x}, {z}) fell outside its own block {coord:?}"
            );
        }
    }

    #[test]
    fn progress_is_reported_in_units_someone_is_waiting_on() {
        // A loading bar may only ever move forward, must finish exactly at
        // its own total, and must count the work actually being done. The
        // old implementation satisfied none of that: it announced 0 and then
        // `total`, so the bar sat still through the entire wait — and the
        // `total` it announced was the block's 256 interior regions while it
        // went on to plan the surrounding halo ring too, so even an honest
        // bar built on it would have stalled at 100%.
        let noise = SimpleNoiseProvider::new();
        let mut samples = Vec::new();
        let mut progress = |done, total| samples.push((done, total));
        WorldBlock::load(
            42,
            BlockCoord { x: 0, z: 0 },
            &config(),
            &noise,
            &mut progress,
        );

        let (_, total) = samples[0];
        assert!(
            samples.iter().all(|&(_, t)| t == total),
            "the total moved under the bar"
        );
        assert!(
            total >= (REGIONS_PER_BLOCK * REGIONS_PER_BLOCK) as usize,
            "a block reported less work than it has regions: {total}"
        );
        assert_eq!(samples.first().map(|&(done, _)| done), Some(0));
        assert_eq!(samples.last(), Some(&(total, total)));
        assert_eq!(
            samples.len(),
            total + 1,
            "progress should be reported once per region planned, plus the opening zero"
        );
        assert!(
            samples.windows(2).all(|w| w[1].0 >= w[0].0),
            "progress went backwards"
        );
    }

    #[test]
    fn the_interior_asks_for_nothing() {
        let noise = SimpleNoiseProvider::new();
        let mut progress = |_, _| {};
        let block = WorldBlock::load(
            42,
            BlockCoord { x: 0, z: 0 },
            &config(),
            &noise,
            &mut progress,
        );
        let middle = Position::new(BLOCK_SIZE * 0.5, BLOCK_SIZE * 0.5);
        assert!(
            block.preload_targets(middle).is_empty(),
            "a player in the middle of a block triggered background work"
        );
    }

    #[test]
    fn approaching_an_edge_asks_for_the_neighbour() {
        let noise = SimpleNoiseProvider::new();
        let mut progress = |_, _| {};
        let block = WorldBlock::load(
            7,
            BlockCoord { x: 0, z: 0 },
            &config(),
            &noise,
            &mut progress,
        );
        let near_east = Position::new(BLOCK_SIZE - 10.0, BLOCK_SIZE * 0.5);
        assert_eq!(
            block.preload_targets(near_east),
            vec![BlockCoord { x: 1, z: 0 }]
        );
    }

    #[test]
    fn a_corner_asks_for_the_diagonal_too() {
        let noise = SimpleNoiseProvider::new();
        let mut progress = |_, _| {};
        let block = WorldBlock::load(
            7,
            BlockCoord { x: 0, z: 0 },
            &config(),
            &noise,
            &mut progress,
        );
        // Heading for a corner, a player can enter the diagonal block
        // without ever crossing either edge neighbour.
        let corner = Position::new(BLOCK_SIZE - 10.0, BLOCK_SIZE - 10.0);
        let targets = block.preload_targets(corner);
        assert!(targets.contains(&BlockCoord { x: 1, z: 1 }), "{targets:?}");
        assert!(targets.contains(&BlockCoord { x: 1, z: 0 }), "{targets:?}");
        assert!(targets.contains(&BlockCoord { x: 0, z: 1 }), "{targets:?}");
    }

    #[test]
    fn a_loaded_block_answers_for_every_point_inside_it() {
        let noise = SimpleNoiseProvider::new();
        let mut progress = |_, _| {};
        let coord = BlockCoord { x: 1, z: -1 };
        let block = WorldBlock::load(42, coord, &config(), &noise, &mut progress);
        let origin = coord.origin();
        // Sample a lattice across the block, including both edges.
        for iz in 0..=8 {
            for ix in 0..=8 {
                let p = Position::new(
                    origin.x + BLOCK_SIZE * ix as f32 / 8.0,
                    origin.z + BLOCK_SIZE * iz as f32 / 8.0,
                );
                assert!(
                    block.plan_at(p).is_some(),
                    "no plan at ({}, {}) inside its own block",
                    p.x,
                    p.z
                );
            }
        }
    }

    #[test]
    fn neighbouring_blocks_agree_across_the_seam() {
        // The whole point of planning per region on a world lattice: a block
        // boundary must not be visible. Both blocks must describe the same
        // building where they meet.
        let noise = SimpleNoiseProvider::new();
        let mut progress = |_, _| {};
        let left = WorldBlock::load(
            42,
            BlockCoord { x: 0, z: 0 },
            &config(),
            &noise,
            &mut progress,
        );
        let right = WorldBlock::load(
            42,
            BlockCoord { x: 1, z: 0 },
            &config(),
            &noise,
            &mut progress,
        );

        // A strip inside the right block, within the left block's halo.
        let mut compared = 0;
        for k in 0..12 {
            let p = Position::new(BLOCK_SIZE + 4.0, k as f32 * 90.0 + 5.0);
            if p.z >= BLOCK_SIZE {
                break;
            }
            let (Some(a), Some(b)) = (left.plan_at(p), right.plan_at(p)) else {
                continue;
            };
            assert_eq!(
                a.assemblies.len(),
                b.assemblies.len(),
                "blocks disagree about the building at ({}, {})",
                p.x,
                p.z
            );
            assert_eq!(a.origin_world.x, b.origin_world.x);
            assert_eq!(a.origin_world.z, b.origin_world.z);
            compared += 1;
        }
        assert!(compared > 0, "the seam was never actually sampled");
    }

    /// The load-bearing equivalence: a chunk voxelized from a block's plans
    /// must be byte-identical to the same chunk streamed the old way. If
    /// this ever fails, bulk loading has changed the world rather than
    /// merely deciding when it was planned.
    #[test]
    fn a_chunk_from_block_plans_matches_the_streamed_chunk() {
        use crate::domain::entities::anomaly::RealitySnapshot;
        use crate::use_cases::level_generator::LevelGenerator;
        use crate::use_cases::level_zero::BackroomsLevel;

        let noise = SimpleNoiseProvider::new();
        let config = config();
        let mut progress = |_, _| {};
        let coord = BlockCoord { x: 0, z: 0 };
        let block = WorldBlock::load(42, coord, &config, &noise, &mut progress);
        let reality = RealitySnapshot::empty();

        for (ox, oz) in [(120.0, 200.0), (640.0, 640.0), (30.0, 1000.0)] {
            let at = Position::new(ox, oz);
            let streamed =
                BackroomsLevel.generate_with_reality(at, 42, config.clone(), &noise, &reality);
            let from_block = BackroomsLevel::generate_from_plans(
                at,
                42,
                config.clone(),
                &noise,
                &reality,
                Some(block.plans()),
            );
            let mut differences = 0usize;
            for z in 0..streamed.grid.depth() {
                for y in 0..streamed.grid.height() {
                    for x in 0..streamed.grid.width() {
                        if streamed.grid.get(x, y, z) != from_block.grid.get(x, y, z) {
                            differences += 1;
                        }
                    }
                }
            }
            assert_eq!(
                differences, 0,
                "block-planned chunk at ({ox}, {oz}) differs from the streamed one \
                 in {differences} voxels"
            );
        }
    }

}
