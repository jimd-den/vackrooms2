//! Direct-to-SVO emission: build the octree straight from a sampling
//! function, with no dense intermediate 3D array.
//!
//! The classic pipeline allocates a dense grid (megabytes per chunk), fills
//! it, then compresses to an SVO. On low-memory targets that intermediate is
//! pure waste when the source can already answer "what is at (x, y, z)?" —
//! and better, "is this whole cube uniform?". A sampler that can prove
//! uniformity for a region lets the builder emit one leaf and never descend,
//! so cost tracks the *surface* of the geometry, not its volume.
//!
//! The arena layout (children contiguous per block, root pushed last,
//! uniform blocks collapsed to a leaf) matches [`BuildOctreeUseCase`]
//! byte-for-byte, so the two paths are interchangeable and comparable with
//! plain equality in tests.

use crate::domain::entities::sparse_voxel_octree::{SparseVoxelOctree, SvoNode};
use crate::domain::entities::voxel_grid::{VOXEL_AIR, VoxelGrid};
use crate::use_cases::ports::MaterialPalette;

/// The attributes of one voxel as the octree stores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvoLeaf {
    pub voxel_type: u8,
    pub color: u32,
    pub light_rgb: [u8; 3],
    pub face_occlusion: u8,
}

impl SvoLeaf {
    pub const AIR: Self = Self {
        voxel_type: VOXEL_AIR,
        color: 0,
        light_rgb: [0; 3],
        face_occlusion: 0,
    };

    fn to_node(self) -> SvoNode {
        SvoNode::Leaf {
            voxel_type: self.voxel_type,
            color: self.color,
            light_rgb: self.light_rgb,
            face_occlusion: self.face_occlusion,
        }
    }
}

/// A volume the builder can sample point-wise, plus an optional proof that a
/// whole cube is uniform (the pruning that makes direct emission cheap).
pub trait VoxelSampler {
    fn sample(&self, x: u32, y: u32, z: u32) -> SvoLeaf;

    /// `Some(leaf)` when every voxel in the cube at `origin` with edge
    /// `size` provably has these attributes. Returning `None` just means
    /// "descend"; it must never be wrong, only conservative.
    fn uniform_hint(&self, _origin: [u32; 3], _size: u32) -> Option<SvoLeaf> {
        None
    }
}

/// Adapts a dense grid (plus presentation palette) to the sampler port.
/// Out-of-grid coordinates are canonical air, and any cube lying entirely
/// past the grid proves itself uniform — the same pruning the dense builder
/// applies.
pub struct GridSampler<'a> {
    pub grid: &'a VoxelGrid,
    pub palette: &'a dyn MaterialPalette,
}

impl VoxelSampler for GridSampler<'_> {
    fn sample(&self, x: u32, y: u32, z: u32) -> SvoLeaf {
        let (x, y, z) = (x as usize, y as usize, z as usize);
        if x >= self.grid.width() || y >= self.grid.height() || z >= self.grid.depth() {
            return SvoLeaf::AIR;
        }
        let voxel_type = self.grid.get(x, y, z);
        if voxel_type == VOXEL_AIR {
            return SvoLeaf::AIR;
        }
        SvoLeaf {
            voxel_type,
            color: self.palette.color(voxel_type),
            light_rgb: self.grid.get_light_rgb(x, y, z),
            face_occlusion: self.grid.get_face_occlusion(x, y, z),
        }
    }

    fn uniform_hint(&self, origin: [u32; 3], _size: u32) -> Option<SvoLeaf> {
        let outside = origin[0] as usize >= self.grid.width()
            || origin[1] as usize >= self.grid.height()
            || origin[2] as usize >= self.grid.depth();
        outside.then_some(SvoLeaf::AIR)
    }
}

/// Builds an SVO of `depth` levels by recursive sampling, emitting leaves
/// for provably uniform cubes without descending into them.
pub fn build_octree_direct(
    sampler: &dyn VoxelSampler,
    depth: u32,
    world_size: f32,
) -> SparseVoxelOctree {
    let mut nodes = Vec::new();
    let root_node = emit_cube(sampler, &mut nodes, [0; 3], 1 << depth, depth);
    let root = nodes.len();
    nodes.push(root_node);
    SparseVoxelOctree {
        root,
        nodes,
        depth,
        world_size,
    }
}

fn emit_cube(
    sampler: &dyn VoxelSampler,
    nodes: &mut Vec<SvoNode>,
    origin: [u32; 3],
    size: u32,
    depth: u32,
) -> SvoNode {
    if let Some(leaf) = sampler.uniform_hint(origin, size) {
        return leaf.to_node();
    }
    if depth == 0 {
        return sampler.sample(origin[0], origin[1], origin[2]).to_node();
    }

    let half = size / 2;
    let mut children = [SvoLeaf::AIR.to_node(); 8];
    for (octant, child) in children.iter_mut().enumerate() {
        let octant = octant as u32;
        let child_origin = [
            origin[0] + (octant & 1) * half,
            origin[1] + ((octant >> 1) & 1) * half,
            origin[2] + ((octant >> 2) & 1) * half,
        ];
        *child = emit_cube(sampler, nodes, child_origin, half, depth - 1);
    }

    if let Some(leaf) = uniform_leaf(&children) {
        return leaf.to_node();
    }

    let child_base_index = nodes.len() as u32;
    let mut child_mask = 0u8;
    for (octant, child) in children.iter().enumerate() {
        let occupied = match child {
            SvoNode::Leaf { voxel_type, .. } => *voxel_type != VOXEL_AIR,
            SvoNode::Internal { .. } => true,
        };
        if occupied {
            child_mask |= 1 << octant;
        }
        nodes.push(*child);
    }
    SvoNode::Internal {
        child_base_index,
        child_mask,
    }
}

/// `Some` when all eight children are leaves with identical attributes.
fn uniform_leaf(children: &[SvoNode; 8]) -> Option<SvoLeaf> {
    let mut first: Option<SvoLeaf> = None;
    for child in children {
        let leaf = match *child {
            SvoNode::Internal { .. } => return None,
            SvoNode::Leaf {
                voxel_type,
                color,
                light_rgb,
                face_occlusion,
            } => SvoLeaf {
                voxel_type,
                color,
                light_rgb,
                face_occlusion,
            },
        };
        match first {
            None => first = Some(leaf),
            Some(seen) if seen != leaf => return None,
            Some(_) => {}
        }
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;
    use crate::domain::entities::voxel_grid::{VOXEL_CEILING, VOXEL_FLOOR, VOXEL_WALL};
    use crate::use_cases::build_octree::BuildOctreeUseCase;
    use std::cell::Cell;

    fn structured_grid() -> VoxelGrid {
        let mut grid = VoxelGrid::new(13, 7, 11);
        for z in 0..11 {
            for x in 0..13 {
                grid.set(x, 0, z, VOXEL_FLOOR);
                grid.set(x, 6, z, VOXEL_CEILING);
                if (x * 7 + z * 3) % 5 == 0 {
                    grid.set(x, 1 + (x + z) % 5, z, VOXEL_WALL);
                    grid.set_light_rgb(x, 1 + (x + z) % 5, z, [(x % 16) as u8, 3, 9]);
                    grid.set_face_occlusion(x, 1 + (x + z) % 5, z, ((x + z) % 64) as u8);
                }
            }
        }
        grid
    }

    #[test]
    fn direct_emission_reproduces_the_dense_builder_arena_exactly() {
        let grid = structured_grid();
        let dense = BuildOctreeUseCase::new(&DEFAULT_MATERIAL_PALETTE).execute(&grid, 4, 16.0);
        let sampler = GridSampler {
            grid: &grid,
            palette: &DEFAULT_MATERIAL_PALETTE,
        };
        let direct = build_octree_direct(&sampler, 4, 16.0);
        assert_eq!(direct, dense, "the two build paths must be interchangeable");
    }

    /// A sampler whose uniformity proof spans the whole lower half of the
    /// cube: the builder must accept the proof and never sample inside it.
    struct HalfSolid {
        samples_inside_proven_region: Cell<u32>,
    }

    impl VoxelSampler for HalfSolid {
        fn sample(&self, _x: u32, y: u32, _z: u32) -> SvoLeaf {
            if y < 8 {
                self.samples_inside_proven_region
                    .set(self.samples_inside_proven_region.get() + 1);
                SvoLeaf {
                    voxel_type: VOXEL_WALL,
                    ..SvoLeaf::AIR
                }
            } else {
                SvoLeaf::AIR
            }
        }

        fn uniform_hint(&self, origin: [u32; 3], size: u32) -> Option<SvoLeaf> {
            if origin[1] + size <= 8 {
                Some(SvoLeaf {
                    voxel_type: VOXEL_WALL,
                    ..SvoLeaf::AIR
                })
            } else if origin[1] >= 8 {
                Some(SvoLeaf::AIR)
            } else {
                None
            }
        }
    }

    #[test]
    fn uniform_hints_prune_descent_entirely() {
        let sampler = HalfSolid {
            samples_inside_proven_region: Cell::new(0),
        };
        let svo = build_octree_direct(&sampler, 4, 16.0);
        assert_eq!(
            sampler.samples_inside_proven_region.get(),
            0,
            "a proven-uniform cube must never be point-sampled"
        );
        assert_eq!(svo.get(3, 3, 3).unwrap().0, VOXEL_WALL);
        assert_eq!(svo.get(3, 12, 3).unwrap().0, VOXEL_AIR);
        // Half-solid/half-air splits once on y: a two-node-deep arena, not a
        // 16^3 expansion.
        assert!(svo.nodes.len() <= 9, "arena grew: {}", svo.nodes.len());
    }
}
