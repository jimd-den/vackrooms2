//! The CAD-voxel marriage: samples a finished CSG solid straight into the
//! sparse octree through [`build_octree_direct`], with no dense grid in
//! between.
//!
//! An authored structure (extruded, cut, combined) becomes a
//! [`SolidSampler`] positioned in chunk-local space, and voxelizes through
//! the same direct SVO path as generated architecture — so player-built
//! geometry and the infinite level share one representation, one renderer
//! upload, and one collision story.

use crate::domain::use_cases::csg::{BspNode, Solid, v3};
use crate::use_cases::build_octree_direct::{SvoLeaf, VoxelSampler};
use crate::use_cases::ports::MaterialPalette;

/// Adapts one CSG solid to the voxel sampler port. Coordinates map voxel
/// indices to world positions at cell centers; everything inside the solid
/// becomes `material`, everything else air.
pub struct SolidSampler<'a> {
    bsp: BspNode,
    /// World AABB of the solid, for whole-cube air pruning.
    bounds: Option<([f64; 3], [f64; 3])>,
    /// World position of voxel (0, 0, 0)'s minimum corner.
    origin: [f64; 3],
    voxel_size: f64,
    material: u8,
    palette: &'a dyn MaterialPalette,
}

impl<'a> SolidSampler<'a> {
    pub fn new(
        solid: &Solid,
        origin: [f64; 3],
        voxel_size: f64,
        material: u8,
        palette: &'a dyn MaterialPalette,
    ) -> Self {
        Self {
            bsp: solid.bsp(),
            bounds: solid
                .aabb()
                .map(|(lo, hi)| ([lo.x, lo.y, lo.z], [hi.x, hi.y, hi.z])),
            origin,
            voxel_size,
            material,
            palette,
        }
    }

    fn cell_center(&self, x: u32, y: u32, z: u32) -> [f64; 3] {
        [
            self.origin[0] + (f64::from(x) + 0.5) * self.voxel_size,
            self.origin[1] + (f64::from(y) + 0.5) * self.voxel_size,
            self.origin[2] + (f64::from(z) + 0.5) * self.voxel_size,
        ]
    }
}

impl VoxelSampler for SolidSampler<'_> {
    fn sample(&self, x: u32, y: u32, z: u32) -> SvoLeaf {
        let [px, py, pz] = self.cell_center(x, y, z);
        if !self.bsp.contains_point(v3(px, py, pz)) {
            return SvoLeaf::AIR;
        }
        SvoLeaf {
            voxel_type: self.material,
            color: self.palette.color(self.material),
            light_rgb: [0; 3],
            face_occlusion: 0,
        }
    }

    fn uniform_hint(&self, cube_origin: [u32; 3], size: u32) -> Option<SvoLeaf> {
        // A cube of cells wholly outside the solid's AABB is provably air.
        let (lo, hi) = self.bounds?;
        let cube_min = self
            .cell_center(cube_origin[0], cube_origin[1], cube_origin[2])
            .map(|c| c - 0.5 * self.voxel_size);
        let extent = f64::from(size) * self.voxel_size;
        let outside =
            (0..3).any(|axis| cube_min[axis] >= hi[axis] || cube_min[axis] + extent <= lo[axis]);
        outside.then_some(SvoLeaf::AIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;
    use crate::domain::entities::cad::{
        CAD_DOOR_HEIGHT, CAD_DOOR_WIDTH, CAD_DOOR_WIDTH_ADA_MIN, fits_human_passage,
    };
    use crate::domain::entities::voxel_grid::{VOXEL_AIR, VOXEL_WALL};
    use crate::use_cases::build_octree_direct::build_octree_direct;

    /// The end-to-end architect flow: extrude a wall, cut a code-compliant
    /// doorway, voxelize directly to an SVO, then verify passability in
    /// voxel space.
    #[test]
    fn authored_doorway_voxelizes_into_a_passable_opening() {
        let voxel_size = 0.1;
        let wall = Solid::cuboid(v3(0.0, 0.0, 2.0), v3(6.0, 3.0, 2.2));
        // Cut the rough opening one voxel wider than the finished width:
        // voxelization quantizes to cell centers, so an exact-width cut can
        // lose up to a voxel of clearance to the jambs. Same practice as
        // framing a real door.
        let rough_half = (CAD_DOOR_WIDTH as f64 + voxel_size) / 2.0;
        let door = Solid::cuboid(
            v3(3.0 - rough_half, 0.0, 1.8),
            v3(3.0 + rough_half, CAD_DOOR_HEIGHT as f64, 2.4),
        );
        let doored_wall = wall.subtract(&door);
        let sampler = SolidSampler::new(
            &doored_wall,
            [0.0, 0.0, 0.0],
            voxel_size,
            VOXEL_WALL,
            &DEFAULT_MATERIAL_PALETTE,
        );
        let svo = build_octree_direct(&sampler, 6, 6.4);

        let at = |wx: f64, wy: f64, wz: f64| {
            svo.get(
                (wx / voxel_size) as u32,
                (wy / voxel_size) as u32,
                (wz / voxel_size) as u32,
            )
            .unwrap()
            .0
        };
        assert_eq!(at(3.0, 1.0, 2.1), VOXEL_AIR, "doorway center is open");
        assert_eq!(at(3.0, 1.9, 2.1), VOXEL_AIR, "head height is open");
        assert_eq!(at(3.0, 2.5, 2.1), VOXEL_WALL, "lintel is solid");
        assert_eq!(at(1.0, 1.0, 2.1), VOXEL_WALL, "jamb wall is solid");

        // Measure the clear opening in voxel space and hold it to code.
        let z = (2.1 / voxel_size) as u32;
        let y = (1.0 / voxel_size) as u32;
        let open_run = (0..64)
            .filter(|&x| svo.get(x, y, z).unwrap().0 == VOXEL_AIR && x > 10 && x < 50)
            .count();
        let clear_width = open_run as f32 * voxel_size as f32;
        assert!(
            clear_width >= CAD_DOOR_WIDTH_ADA_MIN,
            "clear width {clear_width} under ADA minimum"
        );
        assert!(fits_human_passage(clear_width, CAD_DOOR_HEIGHT));
    }

    /// The AABB hint prunes the empty world around a small structure: the
    /// arena must stay far below the full-tree node count.
    #[test]
    fn empty_space_around_a_structure_collapses_without_sampling() {
        let cabinet = Solid::cuboid(v3(1.0, 0.0, 1.0), v3(2.0, 2.0, 2.0));
        let sampler = SolidSampler::new(
            &cabinet,
            [0.0, 0.0, 0.0],
            0.1,
            VOXEL_WALL,
            &DEFAULT_MATERIAL_PALETTE,
        );
        let svo = build_octree_direct(&sampler, 6, 6.4);
        assert_eq!(svo.get(15, 10, 15).unwrap().0, VOXEL_WALL);
        assert_eq!(svo.get(50, 10, 50).unwrap().0, VOXEL_AIR);
        assert!(
            svo.nodes.len() < 20_000,
            "AABB pruning failed: {} nodes",
            svo.nodes.len()
        );
    }
}
