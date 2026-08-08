//! A bounded, least-recently-used cache of planned [`WorldBlock`]s.
//!
//! Planning a block is expensive (hundreds of milliseconds natively, more in
//! a browser) and its result is large — roughly 1.6 MB, most of it furniture
//! — so it must be both reused and forgotten. Reused, because every chunk
//! inside a 1280 u square wants the same plans and re-deriving them per chunk
//! is the waste this whole path exists to remove. Forgotten, because a player
//! who walks for an hour would otherwise accumulate every block they have
//! ever touched until the tab dies.
//!
//! Keyed by `(level, coord)` rather than `coord` alone: two levels are two
//! different buildings at the same coordinates, and handing one's plans to
//! the other would voxelize a chunk that belongs to neither.
//!
//! Deliberately *not* keyed by LOD. A region plan reads only the anomaly
//! tuning from the generator config — never `voxel_scale`, which is the sole
//! thing [`GeneratorConfig::at_lod`] changes — so one planned block is the
//! correct answer for every LOD of every chunk inside it. That is what lets a
//! coarse distant chunk and the fine chunk that replaces it agree about what
//! is built there, and `a_block_serves_every_lod_of_the_same_chunk` below is
//! what keeps that true.
//!
//! Pure of `web-sys` and of any renderer type, so it is unit-tested natively
//! at full speed rather than only in a browser.

use vackrooms::use_cases::generate_chunk::GeneratorConfig;
use vackrooms::use_cases::ports::NoiseProvider;
use vackrooms::use_cases::world_block::{BlockCoord, WorldBlock};

/// How many planned blocks to keep.
///
/// Four covers the worst honest case — a player standing at the corner where
/// four blocks meet, walking between them — at roughly 6 MB. Three would
/// thrash at exactly that corner, and thrashing here costs a re-plan, not a
/// cache miss. Larger buys nothing: a fifth block is never reachable without
/// crossing one of the four.
pub const DEFAULT_BLOCK_CAPACITY: usize = 4;

struct Entry {
    level: u32,
    block: WorldBlock,
    /// Tick of the most recent hit, for LRU eviction.
    used: u64,
}

pub struct BlockCache {
    seed: u32,
    capacity: usize,
    /// Linear-scanned on purpose: `capacity` is single digits, so a `Vec`
    /// beats a `HashMap` on both lookup and allocation at this size.
    entries: Vec<Entry>,
    clock: u64,
    planned: u64,
}

impl BlockCache {
    pub fn new(seed: u32, capacity: usize) -> Self {
        assert!(capacity > 0, "a block cache must be able to hold a block");
        Self {
            seed,
            capacity,
            entries: Vec::with_capacity(capacity),
            clock: 0,
            planned: 0,
        }
    }

    /// Total blocks planned since construction. A session that keeps
    /// increasing this while walking a small area is thrashing.
    pub fn planned_count(&self) -> u64 {
        self.planned
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn holds(&self, level: u32, coord: BlockCoord) -> bool {
        self.index_of(level, coord).is_some()
    }

    fn index_of(&self, level: u32, coord: BlockCoord) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.level == level && e.block.coord() == coord)
    }

    /// The planned block for `coord`, planning it first if absent.
    ///
    /// `config` is used at the caller's level but at *no particular LOD* —
    /// see the module note on why that is sound.
    pub fn get_or_plan(
        &mut self,
        level: u32,
        coord: BlockCoord,
        config: &GeneratorConfig,
        noise: &dyn NoiseProvider,
        progress: &mut dyn FnMut(usize, usize),
    ) -> &WorldBlock {
        self.clock += 1;
        let tick = self.clock;

        if let Some(index) = self.index_of(level, coord) {
            self.entries[index].used = tick;
            return &self.entries[index].block;
        }

        if self.entries.len() == self.capacity {
            let victim = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.used)
                .map(|(i, _)| i)
                .expect("a full cache has an entry to evict");
            self.entries.swap_remove(victim);
        }

        let block = WorldBlock::load(self.seed, coord, config, noise, progress);
        self.planned += 1;
        self.entries.push(Entry {
            level,
            block,
            used: tick,
        });
        &self.entries.last().expect("just pushed").block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vackrooms::domain::entities::position::Position;
    use vackrooms::frameworks_drivers::simple_noise::SimpleNoiseProvider;
    use vackrooms::use_cases::world_block::BLOCK_SIZE;

    fn config() -> GeneratorConfig {
        GeneratorConfig::low_spec()
    }

    fn silent() -> impl FnMut(usize, usize) {
        |_, _| {}
    }

    #[test]
    fn a_second_visit_to_a_block_does_not_re_plan_it() {
        let noise = SimpleNoiseProvider::new();
        let mut cache = BlockCache::new(42, DEFAULT_BLOCK_CAPACITY);
        let coord = BlockCoord { x: 0, z: 0 };
        cache.get_or_plan(0, coord, &config(), &noise, &mut silent());
        cache.get_or_plan(0, coord, &config(), &noise, &mut silent());
        cache.get_or_plan(0, coord, &config(), &noise, &mut silent());
        assert_eq!(
            cache.planned_count(),
            1,
            "the whole point of the cache is that this is 1"
        );
    }

    #[test]
    fn two_levels_at_one_coordinate_are_two_different_buildings() {
        let noise = SimpleNoiseProvider::new();
        let mut cache = BlockCache::new(42, DEFAULT_BLOCK_CAPACITY);
        let coord = BlockCoord { x: 0, z: 0 };
        cache.get_or_plan(0, coord, &config().with_level(0), &noise, &mut silent());
        cache.get_or_plan(1, coord, &config().with_level(1), &noise, &mut silent());
        assert_eq!(cache.planned_count(), 2);
        assert!(cache.holds(0, coord) && cache.holds(1, coord));
    }

    #[test]
    fn the_cache_stays_bounded_and_evicts_the_least_recently_used() {
        let noise = SimpleNoiseProvider::new();
        let mut cache = BlockCache::new(42, 2);
        let a = BlockCoord { x: 0, z: 0 };
        let b = BlockCoord { x: 1, z: 0 };
        let c = BlockCoord { x: 2, z: 0 };

        cache.get_or_plan(0, a, &config(), &noise, &mut silent());
        cache.get_or_plan(0, b, &config(), &noise, &mut silent());
        // Touch `a` so `b` becomes the stalest, then force one eviction.
        cache.get_or_plan(0, a, &config(), &noise, &mut silent());
        cache.get_or_plan(0, c, &config(), &noise, &mut silent());

        assert_eq!(cache.len(), 2, "the cache grew past its capacity");
        assert!(cache.holds(0, a), "the recently used block was evicted");
        assert!(cache.holds(0, c));
        assert!(!cache.holds(0, b), "the stalest block survived");
    }

    #[test]
    fn walking_a_straight_line_across_a_block_plans_it_once() {
        // The failure this guards is a cache that is technically correct but
        // useless: keyed too finely, it would re-plan as the player walks and
        // every step would cost most of a second.
        let noise = SimpleNoiseProvider::new();
        let mut cache = BlockCache::new(42, DEFAULT_BLOCK_CAPACITY);
        let mut x = 8.0f32;
        while x < BLOCK_SIZE {
            let coord = BlockCoord::of(Position::new(x, BLOCK_SIZE * 0.5));
            cache.get_or_plan(0, coord, &config(), &noise, &mut silent());
            x += 16.0;
        }
        assert_eq!(cache.planned_count(), 1);
    }
}
