//! Pooled SVO node atlas: stable, fixed-size chunk slots inside one shared
//! four-word-per-node storage stream.
//!
//! This is the raymarcher's analog of "vertex pooling" in mesh-based voxel
//! engines. The naive approach (concatenate every resident chunk's node
//! array and re-upload the whole GPU buffer on any change) makes every chunk
//! load cost O(world) in CPU copies and transfer bandwidth. The pool instead
//! gives each chunk a fixed row-aligned slot: loading a chunk rebases and
//! uploads *only that slot's rows*, and evicting a chunk is free (its slot is
//! simply reused later; stale nodes are never referenced by the draw table).
//!
//! Every chunk's node array is padded to whole 1024-node rows by the
//! core `OctreeGpuSerializer`, so slots stay row-aligned by construction.
//!
//! `slots` remains the authority for iteration order (`full_texels`,
//! `full_brick_words` walk it slot-by-slot to build one contiguous upload),
//! but `assign`/`release`/`*_offset_of` no longer scan it: `index` and
//! `free` turn "which slot holds this key" and "which slot is free" from an
//! O(n) scan into an O(1) lookup and a stack pop, the first step toward
//! lifting `MAX_CHUNKS` from a hardcoded 25 to a size picked by an actual
//! GPU/storage budget (out-of-core streaming, `VOXEL_RESEARCH_STEAL_LIST.txt`
//! item 3). Raising the cap itself is a separate change: `application::
//! engine`'s per-frame `draws` table and the WebGL2 shader's fixed
//! `uChunkOrigins[25]`-style uniform arrays are their own ceiling on
//! resident chunk count, independent of this pool's slot budget.

use std::collections::HashMap;

use crate::application::ports::ChunkPayload;
use crate::application::streaming::ChunkKey;

/// The raymarch pass's fixed per-frame chunk-table capacity.
pub const MAX_CHUNKS: usize = 25;

/// SVO nodes per atlas row. Must match the raymarch upload offset and the
/// core serializer's row padding.
pub const ROW_TEXELS: usize = 1024;

/// One air-leaf texel (type flag 1, voxel type 0): what free space decodes to.
const AIR_LEAF: [u32; 4] = [1, 0, 0, 0];

/// Brick voxel words per slot row. The dense arena pools on the same slot
/// index as the node arena, so one `assign` places a chunk in both.
pub const BRICK_ROW_WORDS: usize = 1024;

#[derive(Debug, Default)]
pub struct AtlasPool {
    /// Rows per slot. Grows (forcing a relayout) when a chunk won't fit.
    slot_rows: usize,
    /// Rows per slot in the brick voxel arena, sized independently: a chunk
    /// that is mostly uniform has many nodes and few bricks, and vice versa.
    brick_slot_rows: usize,
    /// Slot occupancy; index is the slot number. `full_texels`/
    /// `full_brick_words` are the only readers left that need this instead
    /// of `index` -- they must walk every slot, occupied or not, to build
    /// one contiguous upload.
    slots: Vec<Option<ChunkKey>>,
    /// Reverse of `slots`, kept in sync by `assign`/`release`/relayout.
    index: HashMap<ChunkKey, usize>,
    /// Free slot indices, ascending-index-first like the scan `assign` used
    /// to do (`(0..len).rev()` so the smallest index is on top).
    free: Vec<usize>,
}

impl AtlasPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Nodes (texels) per slot.
    pub fn slot_nodes(&self) -> usize {
        self.slot_rows * ROW_TEXELS
    }

    /// Total rows in the pooled texture.
    pub fn total_rows(&self) -> usize {
        self.slot_rows * self.slots.len()
    }

    /// Ensures the pool holds at least `num_slots` slots of at least `rows`
    /// rows each. Returns `true` when the layout changed — every slot
    /// assignment is then dropped and the caller must reassign all resident
    /// chunks and re-upload the whole pool.
    pub fn ensure_layout(&mut self, num_slots: usize, rows: usize) -> bool {
        self.ensure_layout_with_bricks(num_slots, rows, 0)
    }

    /// As [`Self::ensure_layout`], additionally sizing the brick voxel
    /// arena. Either arena outgrowing its slot relayouts both, so the two
    /// stay addressable from one slot index.
    pub fn ensure_layout_with_bricks(
        &mut self,
        num_slots: usize,
        rows: usize,
        brick_rows: usize,
    ) -> bool {
        let changed = rows > self.slot_rows
            || brick_rows > self.brick_slot_rows
            || num_slots > self.slots.len();
        if changed {
            self.slot_rows = self.slot_rows.max(rows);
            self.brick_slot_rows = self.brick_slot_rows.max(brick_rows);
            let len = self.slots.len().max(num_slots);
            self.slots.clear();
            self.slots.resize(len, None);
            self.index.clear();
            self.free = (0..len).rev().collect();
        }
        changed
    }

    /// Brick voxel words per slot.
    pub fn brick_slot_words(&self) -> usize {
        self.brick_slot_rows * BRICK_ROW_WORDS
    }

    /// Word offset of `key`'s brick block within the pooled voxel arena.
    pub fn brick_offset_of(&self, key: ChunkKey) -> Option<usize> {
        self.index.get(&key).map(|&i| i * self.brick_slot_words())
    }

    /// Assigns (or finds) the slot for `key`. Returns `None` when the pool
    /// is full — the caller sized it for the streaming radius, so that is a
    /// logic error handled by falling back to a full relayout.
    pub fn assign(&mut self, key: ChunkKey) -> Option<usize> {
        if let Some(&i) = self.index.get(&key) {
            return Some(i);
        }
        let slot = self.free.pop()?;
        self.slots[slot] = Some(key);
        self.index.insert(key, slot);
        Some(slot)
    }

    pub fn release(&mut self, key: ChunkKey) {
        if let Some(slot) = self.index.remove(&key) {
            self.slots[slot] = None;
            self.free.push(slot);
        }
    }

    /// First texture row of a slot.
    pub fn slot_first_row(&self, slot: usize) -> usize {
        slot * self.slot_rows
    }

    /// Node offset of `key`'s slot within the pooled atlas.
    pub fn node_offset_of(&self, key: ChunkKey) -> Option<usize> {
        self.index.get(&key).map(|&i| i * self.slot_nodes())
    }

    /// Whether `payload` fits in the current slot size.
    pub fn fits(&self, payload: &ChunkPayload) -> bool {
        payload.nodes.len() / 4 <= self.slot_nodes()
    }

    /// The slot-sized texel block for one chunk: child pointers rebased to
    /// the slot's node offset, tail padded with air leaves.
    pub fn rebased_block(&self, slot: usize, payload: &ChunkPayload) -> Vec<u32> {
        let node_offset = (slot * self.slot_nodes()) as u32;
        let brick_offset = (slot * self.brick_slot_words()) as u32;
        let mut block = Vec::with_capacity(self.slot_nodes() * 4);
        block.extend_from_slice(&payload.nodes);
        // Internal nodes (type flag 0) store child indices local to the
        // chunk, and brick nodes (type flag 2) a word offset into the
        // chunk's own voxel arena; shift both into pool space. Leaf and
        // uniform nodes (flag 1) hold values, not pointers, and must not
        // be touched -- their payload word is a voxel type.
        for i in 0..(payload.nodes.len() / 4) {
            match block[i * 4] {
                0 => block[i * 4 + 1] += node_offset,
                2 => block[i * 4 + 1] += brick_offset,
                _ => {}
            }
        }
        while block.len() < self.slot_nodes() * 4 {
            block.extend_from_slice(&AIR_LEAF);
        }
        block
    }

    /// The whole pool as one texel stream (initial upload, relayouts, and
    /// back ends without partial-update support). `lookup` resolves a slot's
    /// chunk key to its payload.
    pub fn full_texels<'a>(
        &self,
        lookup: impl Fn(ChunkKey) -> Option<&'a ChunkPayload>,
    ) -> Vec<u32> {
        let mut texels = Vec::with_capacity(self.total_rows() * ROW_TEXELS * 4);
        for (slot, occupant) in self.slots.iter().enumerate() {
            match occupant.and_then(&lookup) {
                Some(payload) => texels.extend_from_slice(&self.rebased_block(slot, payload)),
                None => {
                    for _ in 0..self.slot_nodes() {
                        texels.extend_from_slice(&AIR_LEAF);
                    }
                }
            }
        }
        texels
    }

    /// The whole brick voxel arena as one word stream, laid out on the same
    /// slots as [`Self::full_texels`]. Unoccupied slots and the tail of each
    /// occupied one are zero, which decodes as air -- but nothing ever reads
    /// there, because a voxel is only reachable through a word offset some
    /// brick node handed out.
    pub fn full_brick_words<'a>(
        &self,
        lookup: impl Fn(ChunkKey) -> Option<&'a ChunkPayload>,
    ) -> Vec<u32> {
        let stride = self.brick_slot_words();
        let mut words = vec![0u32; stride * self.slots.len()];
        if stride == 0 {
            return words;
        }
        for (slot, occupant) in self.slots.iter().enumerate() {
            if let Some(payload) = occupant.and_then(&lookup) {
                let base = slot * stride;
                words[base..base + payload.brick_voxels.len()]
                    .copy_from_slice(&payload.brick_voxels);
            }
        }
        words
    }
}

/// Rows needed to hold a payload's node array.
pub fn payload_rows(payload: &ChunkPayload) -> usize {
    (payload.nodes.len() / 4).div_ceil(ROW_TEXELS)
}

/// Rows needed to hold a payload's brick voxel arena.
pub fn payload_brick_rows(payload: &ChunkPayload) -> usize {
    payload.brick_voxels.len().div_ceil(BRICK_ROW_WORDS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::streaming::chunk_key;

    fn payload(root: u32, node_count: usize) -> ChunkPayload {
        ChunkPayload {
            root,
            nodes: vec![0; node_count * 4],
            brick_voxels: Vec::new(),
            world_size: 12.8,
            voxel_size: 0.2,
            svo_depth: 6,
            surface: crate::application::ports::SurfaceMeshPayload::empty(0),
            lights: vec![],
            collision: vec![],
            traversal_gates: vec![],
            pit_hazards: vec![],
            supply_items: vec![],
            level_exits: vec![],
        }
    }

    #[test]
    fn slots_are_stable_across_unrelated_evictions() {
        let mut pool = AtlasPool::new();
        pool.ensure_layout(4, 2);
        let (a, b, c) = (
            chunk_key(0.0, 0.0),
            chunk_key(10.0, 0.0),
            chunk_key(20.0, 0.0),
        );
        let slot_a = pool.assign(a).unwrap();
        let slot_b = pool.assign(b).unwrap();
        pool.release(a);
        let slot_c = pool.assign(c).unwrap();
        // b never moved; c reused a's freed slot.
        assert_eq!(pool.node_offset_of(b), Some(slot_b * pool.slot_nodes()));
        assert_eq!(slot_c, slot_a);
    }

    #[test]
    fn rebased_block_shifts_internal_child_pointers_only() {
        let mut pool = AtlasPool::new();
        pool.ensure_layout(2, 1);

        let mut p = payload(0, 2);
        // node 0: internal, child_base 8; node 1: leaf, voxel_type 42.
        p.nodes[0] = 0;
        p.nodes[1] = 8;
        p.nodes[4] = 1;
        p.nodes[5] = 42;

        let block = pool.rebased_block(1, &p);
        assert_eq!(block.len(), pool.slot_nodes() * 4);
        assert_eq!(block[1], 8 + pool.slot_nodes() as u32);
        assert_eq!(block[4], 1, "leaf type flag untouched");
        assert_eq!(block[5], 42, "leaf payload untouched");
        // Padding decodes as air leaves.
        assert_eq!(&block[8..12], &AIR_LEAF);
    }

    #[test]
    fn growing_the_layout_invalidates_assignments() {
        let mut pool = AtlasPool::new();
        assert!(pool.ensure_layout(2, 1));
        let key = chunk_key(0.0, 0.0);
        pool.assign(key).unwrap();
        assert!(!pool.ensure_layout(2, 1), "same layout is a no-op");
        assert!(pool.ensure_layout(2, 3), "bigger slots relayout");
        assert_eq!(pool.node_offset_of(key), None);
    }

    #[test]
    fn assign_is_idempotent_and_does_not_consume_a_free_slot() {
        let mut pool = AtlasPool::new();
        pool.ensure_layout(2, 1);
        let key = chunk_key(0.0, 0.0);
        let first = pool.assign(key).unwrap();
        let second = pool.assign(key).unwrap();
        assert_eq!(first, second, "re-assigning a resident key returns its slot");
        // The other slot is still free: a second, different key must fit.
        assert!(pool.assign(chunk_key(10.0, 0.0)).is_some());
    }

    #[test]
    fn pool_is_full_once_every_slot_is_taken() {
        let mut pool = AtlasPool::new();
        pool.ensure_layout(2, 1);
        assert!(pool.assign(chunk_key(0.0, 0.0)).is_some());
        assert!(pool.assign(chunk_key(10.0, 0.0)).is_some());
        assert_eq!(pool.assign(chunk_key(20.0, 0.0)), None);
    }

    /// Interleaves assign/release across every slot several times over,
    /// checking `index` and `slots` never disagree -- the failure mode an
    /// O(1) reverse-index refactor risks that a linear scan structurally
    /// cannot: a stale `index` entry pointing at a slot `slots` disagrees
    /// with, or a freed slot handed out twice.
    #[test]
    fn index_and_slots_stay_consistent_under_churn() {
        let mut pool = AtlasPool::new();
        pool.ensure_layout(5, 1);
        let keys: Vec<ChunkKey> = (0..12)
            .map(|i| chunk_key(i as f32 * 10.0, 0.0))
            .collect();
        let mut resident: Vec<ChunkKey> = Vec::new();

        for (round, &key) in keys.iter().enumerate() {
            if let Some(slot) = pool.assign(key) {
                assert_eq!(pool.node_offset_of(key), Some(slot * pool.slot_nodes()));
                resident.push(key);
            }
            if round % 3 == 2 {
                if let Some(evict) = resident.pop() {
                    pool.release(evict);
                    assert_eq!(pool.node_offset_of(evict), None);
                }
            }
            // Every still-resident key must resolve to a distinct slot.
            let mut offsets: Vec<usize> = resident
                .iter()
                .filter_map(|&k| pool.node_offset_of(k))
                .collect();
            offsets.sort_unstable();
            offsets.dedup();
            assert_eq!(
                offsets.len(),
                resident.iter().filter(|&&k| pool.node_offset_of(k).is_some()).count(),
                "no two resident keys share a slot"
            );
        }
    }

    #[test]
    fn full_texels_covers_every_slot() {
        let mut pool = AtlasPool::new();
        pool.ensure_layout(3, 1);
        let key = chunk_key(0.0, 0.0);
        let slot = pool.assign(key).unwrap();
        let p = payload(0, 4);
        let texels = pool.full_texels(|k| (k == key).then_some(&p));
        assert_eq!(texels.len(), 3 * pool.slot_nodes() * 4);
        // Unassigned slots are air.
        let other_slot = (slot + 1) % 3;
        let base = other_slot * pool.slot_nodes() * 4;
        assert_eq!(&texels[base..base + 4], &AIR_LEAF);
    }
}
