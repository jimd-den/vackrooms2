//! Fold an SVO's lower levels into dense bricks.
//!
//! A pointer-chasing SVO stores one voxel per leaf node, so a ray pays a
//! dependent memory fetch at every level of every step, and the bottom of
//! the tree is where almost all the nodes are: a fully populated 4x4x4
//! subtree costs 1 + 8 + 64 = 73 nodes to describe 64 voxels.
//!
//! Laine & Karras' answer is to stop subdividing early and store a dense
//! *brick* at the leaves. Traversal descends to the brick, then indexes
//! straight into contiguous memory -- no pointers, good locality, and a
//! shape a 3D texture can hold. That is the representation the "infinite
//! voxel" demos rely on, and it is the prerequisite for shrinking voxels
//! without the node count exploding.
//!
//! This is derived from a finished [`SparseVoxelOctree`] rather than
//! replacing it, exactly as `compress_svdag` is: the SVO stays the
//! authority for collision, editing and the world-identity digest, and
//! only the GPU upload path consumes bricks.

use crate::domain::entities::sparse_voxel_octree::{SparseVoxelOctree, SvoNode};

/// Voxels along one brick edge.
///
/// A brick is dense, so it stores air explicitly and the right edge length
/// is decided by how sparse the world actually is. Measured on a real
/// Level 0 chunk (seed 42, `low_spec`), against a 16209-node / 256 KiB SVO:
///
/// | edge | nodes | GPU     | brick occupancy |
/// |------|-------|---------|-----------------|
/// | 8    |   297 | 440 KiB | 13.1% solid     |
/// | 4    |  1169 | 227 KiB | 27.4% solid     |
///
/// 8^3 buys a further 4x node cut for 1.9x the memory -- a bad trade in a
/// corridor world that is mostly air. 4^3 still removes 93% of the nodes
/// while coming in *under* the SVO it replaces, so it wins outright.
/// Revisit if voxels shrink: finer voxels raise occupancy per brick and
/// shift the balance back toward 8^3.
pub const BRICK_EDGE: u32 = 4;
/// Octree levels a brick replaces (`BRICK_EDGE == 1 << BRICK_DEPTH`).
pub const BRICK_DEPTH: u32 = 2;
/// Voxels held by one brick.
pub const BRICK_VOXELS: usize = (BRICK_EDGE * BRICK_EDGE * BRICK_EDGE) as usize;
/// `u32`s per voxel in the brick arena.
pub const WORDS_PER_VOXEL: usize = 2;
/// `u32`s occupied by one brick.
pub const WORDS_PER_BRICK: usize = BRICK_VOXELS * WORDS_PER_VOXEL;

/// One node of the bricked hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrickNode {
    /// Eight children stored contiguously from `child_base`. `child_mask`
    /// bit `i` is clear when child `i` is entirely air.
    Internal { child_base: u32, child_mask: u8 },
    /// A dense `BRICK_EDGE^3` block starting at `word_base` in the arena.
    Brick { word_base: u32 },
    /// A subtree that is one repeated voxel. Storing it as a node instead
    /// of a brick is what keeps solid walls and open air nearly free.
    Uniform {
        voxel_type: u8,
        color: u32,
        light_rgb: [u8; 3],
        face_occlusion: u8,
    },
}

/// A bricked view of an octree, ready for GPU upload.
#[derive(Debug, Clone, PartialEq)]
pub struct BrickPool {
    pub root: u32,
    pub nodes: Vec<BrickNode>,
    /// Packed voxel payloads, `WORDS_PER_BRICK` per brick.
    ///
    /// word 0: `color` in bits 0..24, `voxel_type` in bits 24..32
    /// word 1: `face_occlusion` in bits 0..8, then r/g/b light nibbles at
    ///         bits 8, 12 and 16
    pub voxels: Vec<u32>,
    pub depth: u32,
    pub world_size: f32,
}

impl BrickPool {
    pub fn brick_count(&self) -> usize {
        self.voxels.len() / WORDS_PER_BRICK
    }

    /// Bytes the GPU upload would occupy, for comparison against the
    /// four-words-per-node encoding this replaces.
    pub fn gpu_bytes(&self) -> usize {
        self.nodes.len() * 4 * 4 + self.voxels.len() * 4
    }

    /// Reads back a voxel, so the pool can be proven equivalent to the
    /// octree it came from.
    pub fn get(&self, x: u32, y: u32, z: u32) -> Option<(u8, u32, [u8; 3], u8)> {
        let extent = 1u32 << self.depth;
        if x >= extent || y >= extent || z >= extent {
            return None;
        }
        Some(self.get_from(self.root as usize, x, y, z, self.depth))
    }

    fn get_from(&self, node: usize, x: u32, y: u32, z: u32, depth: u32) -> (u8, u32, [u8; 3], u8) {
        match self.nodes[node] {
            BrickNode::Uniform {
                voxel_type,
                color,
                light_rgb,
                face_occlusion,
            } => (voxel_type, color, light_rgb, face_occlusion),
            BrickNode::Brick { word_base } => {
                let index = brick_offset(x, y, z);
                let base = word_base as usize + index * WORDS_PER_VOXEL;
                unpack_voxel(self.voxels[base], self.voxels[base + 1])
            }
            BrickNode::Internal {
                child_base,
                child_mask,
            } => {
                let bit = depth - 1;
                let octant = octant_index(x, y, z, bit);
                if child_mask & (1 << octant) == 0 {
                    return (0, 0, [0; 3], 0);
                }
                let mask = (1 << bit) - 1;
                self.get_from(
                    child_base as usize + octant as usize,
                    x & mask,
                    y & mask,
                    z & mask,
                    depth - 1,
                )
            }
        }
    }
}

fn octant_index(x: u32, y: u32, z: u32, bit: u32) -> u32 {
    (((z >> bit) & 1) << 2) | (((y >> bit) & 1) << 1) | ((x >> bit) & 1)
}

/// Brick-local linear offset. X varies fastest so a ray stepping along X
/// walks contiguous words.
fn brick_offset(x: u32, y: u32, z: u32) -> usize {
    let edge = BRICK_EDGE;
    ((z % edge) * edge * edge + (y % edge) * edge + (x % edge)) as usize
}

fn pack_voxel(voxel_type: u8, color: u32, light_rgb: [u8; 3], face_occlusion: u8) -> (u32, u32) {
    let word0 = (color & 0x00FF_FFFF) | ((voxel_type as u32) << 24);
    let word1 = face_occlusion as u32
        | ((light_rgb[0] as u32 & 0xF) << 8)
        | ((light_rgb[1] as u32 & 0xF) << 12)
        | ((light_rgb[2] as u32 & 0xF) << 16);
    (word0, word1)
}

fn unpack_voxel(word0: u32, word1: u32) -> (u8, u32, [u8; 3], u8) {
    let voxel_type = (word0 >> 24) as u8;
    let color = word0 & 0x00FF_FFFF;
    let light_rgb = [
        ((word1 >> 8) & 0xF) as u8,
        ((word1 >> 12) & 0xF) as u8,
        ((word1 >> 16) & 0xF) as u8,
    ];
    (voxel_type, color, light_rgb, (word1 & 0xFF) as u8)
}

/// Builds a [`BrickPool`] from a finished octree.
pub fn build_brick_pool(octree: &SparseVoxelOctree) -> BrickPool {
    let mut pool = BrickPool {
        root: 0,
        nodes: Vec::new(),
        voxels: Vec::new(),
        depth: octree.depth,
        world_size: octree.world_size,
    };
    // The root occupies slot 0 so `child_base` runs are contiguous and a
    // subtree can be written into a slot its parent reserved in advance.
    push(&mut pool, EMPTY);
    emit_into(&mut pool, octree, octree.root, octree.depth, 0);
    pool.root = 0;
    pool
}

const EMPTY: BrickNode = BrickNode::Uniform {
    voxel_type: 0,
    color: 0,
    light_rgb: [0; 3],
    face_occlusion: 0,
};

/// Writes the subtree rooted at `node` into the already-reserved `slot`.
///
/// Children are reserved as one contiguous run *before* they are filled,
/// so `child_base + octant` stays a direct offset and no node is ever
/// emitted twice.
fn emit_into(
    pool: &mut BrickPool,
    octree: &SparseVoxelOctree,
    node: usize,
    depth: u32,
    slot: usize,
) {
    if let Some((voxel_type, color, light_rgb, face_occlusion)) = uniform_value(octree, node) {
        pool.nodes[slot] = BrickNode::Uniform {
            voxel_type,
            color,
            light_rgb,
            face_occlusion,
        };
        return;
    }

    if depth <= BRICK_DEPTH {
        // Split the borrows: the brick is built against the voxel arena
        // before the node slot is written.
        let brick = emit_brick(&mut pool.voxels, octree, node, depth);
        pool.nodes[slot] = brick;
        return;
    }

    let SvoNode::Internal {
        child_base_index,
        child_mask,
    } = octree.nodes[node]
    else {
        unreachable!("uniform_value already handled every leaf");
    };

    let child_base = pool.nodes.len() as u32;
    // Eight slots are reserved even for absent children so the octant
    // index stays a direct offset; absent ones remain air.
    for _ in 0..8 {
        push(pool, EMPTY);
    }
    pool.nodes[slot] = BrickNode::Internal {
        child_base,
        child_mask,
    };
    for octant in 0..8u32 {
        if child_mask & (1 << octant) == 0 {
            continue;
        }
        emit_into(
            pool,
            octree,
            child_base_index as usize + octant as usize,
            depth - 1,
            child_base as usize + octant as usize,
        );
    }
}

fn emit_brick(
    voxels: &mut Vec<u32>,
    octree: &SparseVoxelOctree,
    node: usize,
    depth: u32,
) -> BrickNode {
    let word_base = voxels.len() as u32;
    voxels.resize(voxels.len() + WORDS_PER_BRICK, 0);
    // A subtree shallower than one brick only occupies the low corner;
    // the rest of the brick stays air rather than tiling the subtree.
    let extent = 1u32 << depth;
    for z in 0..BRICK_EDGE.min(extent) {
        for y in 0..BRICK_EDGE.min(extent) {
            for x in 0..BRICK_EDGE.min(extent) {
                let voxel = sample(octree, node, x, y, z, depth);
                let (w0, w1) = pack_voxel(voxel.0, voxel.1, voxel.2, voxel.3);
                let slot = word_base as usize + brick_offset(x, y, z) * WORDS_PER_VOXEL;
                voxels[slot] = w0;
                voxels[slot + 1] = w1;
            }
        }
    }
    BrickNode::Brick { word_base }
}

/// Reads a voxel out of an octree subtree in that subtree's local frame.
fn sample(
    octree: &SparseVoxelOctree,
    node: usize,
    x: u32,
    y: u32,
    z: u32,
    depth: u32,
) -> (u8, u32, [u8; 3], u8) {
    match octree.nodes[node] {
        SvoNode::Leaf {
            voxel_type,
            color,
            light_rgb,
            face_occlusion,
        } => (voxel_type, color, light_rgb, face_occlusion),
        SvoNode::Internal {
            child_base_index,
            child_mask,
        } => {
            if depth == 0 {
                return (0, 0, [0; 3], 0);
            }
            let bit = depth - 1;
            let octant = octant_index(x, y, z, bit);
            if child_mask & (1 << octant) == 0 {
                return (0, 0, [0; 3], 0);
            }
            let mask = (1 << bit) - 1;
            sample(
                octree,
                child_base_index as usize + octant as usize,
                x & mask,
                y & mask,
                z & mask,
                depth - 1,
            )
        }
    }
}

/// `Some` when the subtree is a single repeated voxel.
fn uniform_value(octree: &SparseVoxelOctree, node: usize) -> Option<(u8, u32, [u8; 3], u8)> {
    match octree.nodes[node] {
        SvoNode::Leaf {
            voxel_type,
            color,
            light_rgb,
            face_occlusion,
        } => Some((voxel_type, color, light_rgb, face_occlusion)),
        SvoNode::Internal { child_mask, .. } if child_mask == 0 => Some((0, 0, [0; 3], 0)),
        SvoNode::Internal { .. } => None,
    }
}

fn push(pool: &mut BrickPool, node: BrickNode) -> u32 {
    pool.nodes.push(node);
    (pool.nodes.len() - 1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random fill: enough structure that whole
    /// subtrees collapse to `Uniform`, enough noise that bricks are real.
    fn populated_octree(depth: u32) -> SparseVoxelOctree {
        let mut octree = SparseVoxelOctree::new(depth, 10.0);
        let extent = 1u32 << depth;
        for z in 0..extent {
            for y in 0..extent {
                for x in 0..extent {
                    // A slab of solid, a void, and a noisy band, so all
                    // three node kinds are exercised.
                    let solid =
                        y < extent / 4 || (y > extent / 2 && (x * 7 + y * 13 + z * 29) % 5 == 0);
                    if !solid {
                        continue;
                    }
                    let material = ((x + y + z) % 7 + 1) as u8;
                    octree.set(
                        x,
                        y,
                        z,
                        material,
                        0x00AB_CDEF ^ (x * 131 + z * 17),
                        [(x % 16) as u8, (y % 16) as u8, (z % 16) as u8],
                        (x % 64) as u8,
                    );
                }
            }
        }
        octree
    }

    #[test]
    fn every_voxel_survives_the_round_trip() {
        for depth in [4, 5, 6] {
            let octree = populated_octree(depth);
            let pool = build_brick_pool(&octree);
            assert_eq!(pool.depth, octree.depth);
            let extent = 1u32 << depth;
            for z in 0..extent {
                for y in 0..extent {
                    for x in 0..extent {
                        assert_eq!(
                            pool.get(x, y, z),
                            octree.get(x, y, z),
                            "voxel ({x},{y},{z}) diverged at depth {depth}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn bricks_collapse_the_bottom_of_the_tree() {
        let octree = populated_octree(6);
        let pool = build_brick_pool(&octree);
        // The whole point: a brick replaces the 585 nodes a populated
        // 8^3 subtree would otherwise need.
        assert!(
            pool.nodes.len() < octree.nodes.len() / 4,
            "expected a large node reduction, got {} from {}",
            pool.nodes.len(),
            octree.nodes.len()
        );
        assert!(pool.brick_count() > 0, "a noisy world must produce bricks");
    }

    #[test]
    fn uniform_subtrees_cost_no_brick() {
        // Entirely air: no brick should ever be allocated.
        let empty = SparseVoxelOctree::new(6, 10.0);
        let pool = build_brick_pool(&empty);
        assert_eq!(pool.brick_count(), 0);
        assert_eq!(pool.nodes.len(), 1);
        assert_eq!(pool.get(0, 0, 0), Some((0, 0, [0; 3], 0)));
    }

    #[test]
    fn coordinates_outside_the_volume_are_rejected() {
        let pool = build_brick_pool(&populated_octree(4));
        assert_eq!(pool.get(16, 0, 0), None);
        assert_eq!(pool.get(0, 99, 0), None);
    }

    #[test]
    fn packing_survives_every_field() {
        let (w0, w1) = pack_voxel(0xAB, 0x00FF_1234, [1, 9, 15], 0x3F);
        assert_eq!(unpack_voxel(w0, w1), (0xAB, 0x00FF_1234, [1, 9, 15], 0x3F));
    }
}
