//! Sparse Voxel DAG compression: bottom-up merging of identical subtrees.
//!
//! The SVO arena stores every internal node's eight children as one
//! contiguous block, so the unit of sharing is that block: two internal
//! nodes whose child blocks are structurally identical can point at a single
//! canonical copy. Repetitive architecture (identical ceiling tiles, wall
//! runs, column bays) then costs one subtree instead of hundreds.
//!
//! The result is still a [`SparseVoxelOctree`]: traversal only ever follows
//! `child_base_index`, so lookups and the GPU serializer are agnostic to
//! whether the arena is a tree or a DAG. The one consumer that must NOT
//! receive a DAG is any walk that visits every arena node expecting each to
//! appear once per world position (e.g. collision extraction) — compress
//! after those walks, immediately before upload.

use std::collections::HashMap;

use crate::domain::entities::sparse_voxel_octree::{SparseVoxelOctree, SvoNode};

/// Arena sizes before and after deduplication, for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvdagStats {
    pub nodes_before: usize,
    pub nodes_after: usize,
}

/// A node in canonical form: internals are identified by the id of their
/// interned child block rather than an arena offset, which is what makes
/// structural equality (and therefore hashing) meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CanonNode {
    Leaf {
        voxel_type: u8,
        color: u32,
        light_rgb: [u8; 3],
        face_occlusion: u8,
    },
    Internal {
        child_base_index: u32,
        child_mask: u8,
    },
}

impl CanonNode {
    fn to_svo(self) -> SvoNode {
        match self {
            CanonNode::Leaf {
                voxel_type,
                color,
                light_rgb,
                face_occlusion,
            } => SvoNode::Leaf {
                voxel_type,
                color,
                light_rgb,
                face_occlusion,
            },
            CanonNode::Internal {
                child_base_index,
                child_mask,
            } => SvoNode::Internal {
                child_base_index,
                child_mask,
            },
        }
    }
}

struct BlockInterner {
    /// Canonical child block -> its base index in the output arena.
    blocks: HashMap<[CanonNode; 8], u32>,
    nodes: Vec<SvoNode>,
}

impl BlockInterner {
    /// Returns the output-arena base of this block, materializing it only
    /// the first time the block's structure is seen.
    fn intern(&mut self, block: [CanonNode; 8]) -> u32 {
        if let Some(&base) = self.blocks.get(&block) {
            return base;
        }
        let base = self.nodes.len() as u32;
        self.nodes.extend(block.iter().map(|canon| canon.to_svo()));
        self.blocks.insert(block, base);
        base
    }

    /// Rebuilds the subtree rooted at `index` bottom-up, sharing every child
    /// block already interned.
    fn canonicalize(&mut self, source: &SparseVoxelOctree, index: usize) -> CanonNode {
        match source.nodes[index] {
            SvoNode::Leaf {
                voxel_type,
                color,
                light_rgb,
                face_occlusion,
            } => CanonNode::Leaf {
                voxel_type,
                color,
                light_rgb,
                face_occlusion,
            },
            SvoNode::Internal {
                child_base_index,
                child_mask,
            } => {
                let mut block = [CanonNode::Leaf {
                    voxel_type: 0,
                    color: 0,
                    light_rgb: [0; 3],
                    face_occlusion: 0,
                }; 8];
                for (octant, slot) in block.iter_mut().enumerate() {
                    *slot = self.canonicalize(source, child_base_index as usize + octant);
                }
                CanonNode::Internal {
                    child_base_index: self.intern(block),
                    child_mask,
                }
            }
        }
    }
}

/// Deduplicates identical subtrees, returning a lookup-equivalent octree
/// whose arena shares one copy of every repeated child block.
pub fn compress_svdag(source: &SparseVoxelOctree) -> (SparseVoxelOctree, SvdagStats) {
    let mut interner = BlockInterner {
        blocks: HashMap::new(),
        nodes: Vec::new(),
    };
    let root_canon = interner.canonicalize(source, source.root);

    // Root last, matching the tree builder's arena convention.
    let root = interner.nodes.len();
    interner.nodes.push(root_canon.to_svo());

    let stats = SvdagStats {
        nodes_before: source.nodes.len(),
        nodes_after: interner.nodes.len(),
    };
    (
        SparseVoxelOctree {
            root,
            nodes: interner.nodes,
            depth: source.depth,
            world_size: source.world_size,
        },
        stats,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;
    use crate::domain::entities::voxel_grid::{VOXEL_CEILING, VOXEL_FLOOR, VOXEL_WALL, VoxelGrid};
    use crate::use_cases::build_octree::BuildOctreeUseCase;

    /// A floor slab plus a periodic wall/ceiling module: the same subtree
    /// repeats across the grid, which is exactly what SVDAG merging exists
    /// to exploit.
    fn repetitive_grid() -> VoxelGrid {
        let mut grid = VoxelGrid::new(16, 8, 16);
        for z in 0..16 {
            for x in 0..16 {
                grid.set(x, 0, z, VOXEL_FLOOR);
                grid.set(x, 7, z, VOXEL_CEILING);
                if x % 4 == 0 && z % 4 == 0 {
                    for y in 1..7 {
                        grid.set(x, y, z, VOXEL_WALL);
                    }
                }
            }
        }
        grid
    }

    fn build(grid: &VoxelGrid) -> SparseVoxelOctree {
        BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE).execute(grid, 4, 16.0)
    }

    #[test]
    fn every_lookup_survives_compression_unchanged() {
        let svo = build(&repetitive_grid());
        let (dag, _) = compress_svdag(&svo);
        assert_eq!(dag.depth, svo.depth);
        assert_eq!(dag.world_size, svo.world_size);
        for z in 0..16 {
            for y in 0..16 {
                for x in 0..16 {
                    assert_eq!(dag.get(x, y, z), svo.get(x, y, z), "at ({x},{y},{z})");
                }
            }
        }
    }

    #[test]
    fn repeated_modules_share_one_subtree() {
        let svo = build(&repetitive_grid());
        let (_, stats) = compress_svdag(&svo);
        assert_eq!(stats.nodes_before, svo.nodes.len());
        assert!(
            stats.nodes_after * 2 <= stats.nodes_before,
            "expected >=2x arena reduction on periodic architecture, got {} -> {}",
            stats.nodes_before,
            stats.nodes_after
        );
    }

    #[test]
    fn compression_is_idempotent() {
        let svo = build(&repetitive_grid());
        let (once, first) = compress_svdag(&svo);
        let (_, twice) = compress_svdag(&once);
        assert_eq!(twice.nodes_after, first.nodes_after);
    }

    /// The number that matters for the Pi: a real generated Level 0 chunk
    /// must compress meaningfully, or the upload path gained nothing.
    #[test]
    fn a_real_level_zero_chunk_compresses() {
        use crate::domain::entities::position::Position;
        use crate::frameworks_drivers::simple_noise::SimpleNoiseProvider;
        use crate::use_cases::generate_chunk::{GenerateChunkArchitectureUseCase, GeneratorConfig};

        let noise = SimpleNoiseProvider::new();
        let chunk = GenerateChunkArchitectureUseCase::new(&noise).execute(
            Position::new(20.0, 20.0),
            42,
            GeneratorConfig::low_spec(),
        );
        let svo = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE).execute(&chunk, 6, 12.8);
        let (_, stats) = compress_svdag(&svo);
        println!(
            "level 0 chunk SVDAG: {} -> {} nodes ({:.1}%)",
            stats.nodes_before,
            stats.nodes_after,
            100.0 * stats.nodes_after as f64 / stats.nodes_before as f64
        );
        assert!(
            stats.nodes_after < stats.nodes_before,
            "real chunks must share at least some subtrees"
        );
    }

    #[test]
    fn leaf_only_octree_passes_through() {
        let grid = VoxelGrid::new(4, 4, 4);
        let svo = build(&grid);
        let (dag, stats) = compress_svdag(&svo);
        assert_eq!(stats.nodes_after, 1);
        assert_eq!(dag.get(0, 0, 0), svo.get(0, 0, 0));
    }
}
