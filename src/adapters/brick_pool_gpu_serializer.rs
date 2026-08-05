//! Flatten a [`BrickPool`] into the two arenas a shader traverses.
//!
//! The pool is two distinct kinds of memory and they upload separately:
//! a small pointer hierarchy that a ray walks, and a large dense voxel
//! arena that it indexes once it has stopped walking. Keeping them apart
//! is the point of bricking -- the hierarchy stays cache-resident while
//! the arena is streamed.
//!
//! Both arenas are padded to whole 1024-element rows, matching
//! [`super::octree_gpu_serializer`], so a chunk can be re-uploaded into a
//! row-aligned slice of a shared atlas without disturbing its neighbours.

use crate::use_cases::build_brick_pool::{BrickNode, BrickPool, WORDS_PER_BRICK};

/// Elements per atlas row. Shared by both arenas so a linear index maps to
/// a texel with the same divide on either.
pub const ATLAS_WIDTH: usize = 1024;

/// `u32` lanes per hierarchy node.
pub const WORDS_PER_NODE: usize = 4;

/// `w0` discriminants. A shader switches on this before reading `w1..w3`.
pub const NODE_KIND_INTERNAL: u32 = 0;
pub const NODE_KIND_UNIFORM: u32 = 1;
pub const NODE_KIND_BRICK: u32 = 2;

/// Row-padded brick data ready for renderer storage upload.
#[derive(Debug, Clone, PartialEq)]
pub struct BrickPoolGpuData {
    /// Index into `node_data` of the root node, in nodes (not words).
    pub root: u32,
    /// Hierarchy arena, [`WORDS_PER_NODE`] words per node.
    pub node_data: Vec<u32>,
    pub node_texture_width: u32,
    pub node_texture_height: u32,
    /// Dense voxel arena, [`WORDS_PER_BRICK`] words per brick.
    pub voxel_data: Vec<u32>,
    pub voxel_texture_width: u32,
    pub voxel_texture_height: u32,
    pub depth: u32,
    pub world_size: f32,
}

impl BrickPoolGpuData {
    pub fn bytes(&self) -> usize {
        (self.node_data.len() + self.voxel_data.len()) * 4
    }
}

/// GPU adapter for the bricked hierarchy.
///
/// ENCODING (four `u32` lanes per node):
///
/// - **w0:** node kind -- `0` internal, `1` uniform, `2` brick.
/// - **w1:**
///   * internal: `child_base`, the node index of child 0; children 1..7
///     follow contiguously.
///   * uniform: `voxel_type`.
///   * brick: `word_base`, the first word of the brick in the voxel arena.
/// - **w2:**
///   * internal: `child_mask`, bit `i` set when child `i` is not pure air.
///   * uniform: 24-bit packed `0xRRGGBB` colour.
///   * brick: `0`.
/// - **w3:**
///   * internal, brick: `0`.
///   * uniform: packed lighting, bit-identical to the leaf word of
///     [`super::octree_gpu_serializer`] so both paths decode with one
///     shared routine:
///     - bits 0-7:   scalar light level (the max of the RGB channels)
///     - bits 8-13:  neighbour-occupancy mask (+X,-X,+Y,-Y,+Z,-Z);
///       a clear bit is an exposed physical face
///     - bits 16-19: red, 20-23: green, 24-27: blue (0-15 each)
///
/// The voxel arena keeps the pool's own two-word packing untouched -- it
/// is already GPU-shaped, and re-packing it here would put the same
/// layout in two places.
pub struct BrickPoolGpuSerializer;

impl BrickPoolGpuSerializer {
    pub fn serialize_to_gpu_data(pool: &BrickPool) -> BrickPoolGpuData {
        let mut node_data = Vec::with_capacity(pool.nodes.len() * WORDS_PER_NODE);

        for node in &pool.nodes {
            match *node {
                BrickNode::Internal {
                    child_base,
                    child_mask,
                } => {
                    node_data.extend_from_slice(&[
                        NODE_KIND_INTERNAL,
                        child_base,
                        child_mask as u32,
                        0,
                    ]);
                }
                BrickNode::Uniform {
                    voxel_type,
                    color,
                    light_rgb,
                    face_occlusion,
                } => {
                    node_data.extend_from_slice(&[
                        NODE_KIND_UNIFORM,
                        voxel_type as u32,
                        color,
                        pack_light_word(light_rgb, face_occlusion),
                    ]);
                }
                BrickNode::Brick { word_base } => {
                    node_data.extend_from_slice(&[NODE_KIND_BRICK, word_base, 0, 0]);
                }
            }
        }

        // Pad with air-uniform nodes: a stale index landing in the tail
        // reads as empty space rather than as a pointer into nothing.
        let node_rows = pad_to_rows(&mut node_data, WORDS_PER_NODE, |out| {
            out.extend_from_slice(&[NODE_KIND_UNIFORM, 0, 0, 0]);
        });

        let mut voxel_data = pool.voxels.clone();
        // The arena is only ever addressed via a `word_base` a node hands
        // out, so the tail is unreachable; zero is air regardless.
        let voxel_rows = pad_to_rows(&mut voxel_data, WORDS_PER_BRICK, |out| out.push(0));

        BrickPoolGpuData {
            root: pool.root,
            node_data,
            node_texture_width: ATLAS_WIDTH as u32,
            node_texture_height: node_rows,
            voxel_data,
            voxel_texture_width: ATLAS_WIDTH as u32,
            voxel_texture_height: voxel_rows,
            depth: pool.depth,
            world_size: pool.world_size,
        }
    }
}

fn pack_light_word(light_rgb: [u8; 3], face_occlusion: u8) -> u32 {
    let [r, g, b] = light_rgb;
    let scalar = r.max(g).max(b) as u32;
    scalar
        | (face_occlusion as u32) << 8
        | (r as u32 & 0xF) << 16
        | (g as u32 & 0xF) << 20
        | (b as u32 & 0xF) << 24
}

/// Grows `data` to a whole number of [`ATLAS_WIDTH`] rows, appending in
/// `stride`-sized units so padding never splits a record, and returns the
/// row count. `ATLAS_WIDTH` is a multiple of both strides in use, so the
/// two constraints never conflict.
fn pad_to_rows(data: &mut Vec<u32>, stride: usize, mut append: impl FnMut(&mut Vec<u32>)) -> u32 {
    debug_assert_eq!(ATLAS_WIDTH % stride, 0, "a record must not straddle a row");
    let rows = data.len().div_ceil(ATLAS_WIDTH).max(1);
    while data.len() < rows * ATLAS_WIDTH {
        append(data);
    }
    rows as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::sparse_voxel_octree::SparseVoxelOctree;
    use crate::domain::entities::voxel_grid::{FACE_OCCLUDED_NEGATIVE_X, FACE_OCCLUDED_POSITIVE_Y};
    use crate::use_cases::build_brick_pool::{build_brick_pool, WORDS_PER_VOXEL};

    /// Walks the serialized arenas exactly as a shader would, with no
    /// access to the Rust types -- if this agrees with the pool, the
    /// encoding is self-describing.
    fn shader_fetch(gpu: &BrickPoolGpuData, x: u32, y: u32, z: u32) -> (u8, u32, [u8; 3], u8) {
        let mut node = gpu.root as usize;
        let mut depth = gpu.depth;
        let (mut x, mut y, mut z) = (x, y, z);
        loop {
            let base = node * WORDS_PER_NODE;
            let w = &gpu.node_data[base..base + WORDS_PER_NODE];
            match w[0] {
                NODE_KIND_UNIFORM => {
                    let light = w[3];
                    return (
                        w[1] as u8,
                        w[2],
                        [
                            ((light >> 16) & 0xF) as u8,
                            ((light >> 20) & 0xF) as u8,
                            ((light >> 24) & 0xF) as u8,
                        ],
                        ((light >> 8) & 0x3F) as u8,
                    );
                }
                NODE_KIND_BRICK => {
                    let edge = 1u32 << depth;
                    let offset = (z * edge * edge + y * edge + x) as usize;
                    let base = w[1] as usize + offset * WORDS_PER_VOXEL;
                    let (v0, v1) = (gpu.voxel_data[base], gpu.voxel_data[base + 1]);
                    return (
                        (v0 >> 24) as u8,
                        v0 & 0x00FF_FFFF,
                        [
                            ((v1 >> 8) & 0xF) as u8,
                            ((v1 >> 12) & 0xF) as u8,
                            ((v1 >> 16) & 0xF) as u8,
                        ],
                        (v1 & 0xFF) as u8,
                    );
                }
                _ => {
                    let bit = depth - 1;
                    let octant = (((z >> bit) & 1) << 2) | (((y >> bit) & 1) << 1) | ((x >> bit) & 1);
                    if w[2] & (1 << octant) == 0 {
                        return (0, 0, [0; 3], 0);
                    }
                    node = w[1] as usize + octant as usize;
                    let mask = (1 << bit) - 1;
                    x &= mask;
                    y &= mask;
                    z &= mask;
                    depth = bit;
                }
            }
        }
    }

    fn speckled_octree(depth: u32) -> SparseVoxelOctree {
        let mut octree = SparseVoxelOctree::new(depth, 8.0);
        let extent = 1u32 << depth;
        for x in 0..extent {
            for y in 0..extent {
                for z in 0..extent {
                    if (x * 7 + y * 13 + z * 29) % 5 == 0 {
                        continue; // leave air, so the tree stays sparse
                    }
                    let n = x + y * 3 + z * 5;
                    octree.set(
                        x,
                        y,
                        z,
                        1 + (n % 4) as u8,
                        (n * 40503) & 0x00FF_FFFF,
                        [(n % 16) as u8, ((n + 5) % 16) as u8, ((n + 11) % 16) as u8],
                        (n % 64) as u8,
                    );
                }
            }
        }
        octree
    }

    #[test]
    fn a_shader_side_walk_reproduces_every_voxel() {
        for depth in [3, 4, 5] {
            let octree = speckled_octree(depth);
            let pool = build_brick_pool(&octree);
            let gpu = BrickPoolGpuSerializer::serialize_to_gpu_data(&pool);

            let extent = 1u32 << depth;
            for x in 0..extent {
                for y in 0..extent {
                    for z in 0..extent {
                        assert_eq!(
                            shader_fetch(&gpu, x, y, z),
                            pool.get(x, y, z).unwrap(),
                            "depth {depth} at ({x}, {y}, {z})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn both_arenas_land_on_whole_rows() {
        let pool = build_brick_pool(&speckled_octree(5));
        let gpu = BrickPoolGpuSerializer::serialize_to_gpu_data(&pool);

        assert_eq!(
            gpu.node_data.len(),
            (gpu.node_texture_width * gpu.node_texture_height) as usize
        );
        assert_eq!(
            gpu.voxel_data.len(),
            (gpu.voxel_texture_width * gpu.voxel_texture_height) as usize
        );
        assert!(gpu.node_data.len() >= pool.nodes.len() * WORDS_PER_NODE);
        assert!(gpu.voxel_data.len() >= pool.voxels.len());
    }

    #[test]
    fn padding_reads_as_air_not_as_a_pointer() {
        let pool = build_brick_pool(&speckled_octree(3));
        let gpu = BrickPoolGpuSerializer::serialize_to_gpu_data(&pool);

        for node in pool.nodes.len()..(gpu.node_data.len() / WORDS_PER_NODE) {
            let base = node * WORDS_PER_NODE;
            assert_eq!(gpu.node_data[base], NODE_KIND_UNIFORM);
            assert_eq!(gpu.node_data[base + 1], 0, "padding must be voxel type air");
        }
    }

    #[test]
    fn a_uniform_light_word_matches_the_octree_encoding() {
        let mask = FACE_OCCLUDED_NEGATIVE_X | FACE_OCCLUDED_POSITIVE_Y;
        let mut octree = SparseVoxelOctree::new(2, 4.0);
        for x in 0..4 {
            for y in 0..4 {
                for z in 0..4 {
                    octree.set(x, y, z, 1, 0xAA8844, [7, 5, 3], mask);
                }
            }
        }

        let pool = build_brick_pool(&octree);
        let gpu = BrickPoolGpuSerializer::serialize_to_gpu_data(&pool);

        // A wholly uniform volume must collapse to a single node, never a brick.
        let root = gpu.root as usize * WORDS_PER_NODE;
        assert_eq!(gpu.node_data[root], NODE_KIND_UNIFORM);
        assert_eq!(gpu.voxel_data.iter().filter(|w| **w != 0).count(), 0);

        let light = gpu.node_data[root + 3];
        assert_eq!(light & 0xFF, 7, "scalar level is the channel max");
        assert_eq!((light >> 8) & 0x3F, mask as u32);
    }

    #[test]
    fn the_hierarchy_arena_is_far_smaller_than_the_octree_it_replaces() {
        let octree = speckled_octree(5);
        let pool = build_brick_pool(&octree);
        let gpu = BrickPoolGpuSerializer::serialize_to_gpu_data(&pool);

        assert!(
            gpu.node_data.len() * 4 < octree.nodes.len() * WORDS_PER_NODE * 4 / 4,
            "bricking should remove the bulk of the nodes, got {} words for {} SVO nodes",
            gpu.node_data.len(),
            octree.nodes.len()
        );
    }
}
