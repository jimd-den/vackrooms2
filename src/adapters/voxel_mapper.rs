use crate::domain::entities::voxel_grid::{
    FACE_OCCLUDED_NEGATIVE_X, FACE_OCCLUDED_NEGATIVE_Y, FACE_OCCLUDED_NEGATIVE_Z,
    FACE_OCCLUDED_POSITIVE_X, FACE_OCCLUDED_POSITIVE_Y, FACE_OCCLUDED_POSITIVE_Z, VOXEL_AIR,
    VOXEL_WALL, VoxelGrid,
};
use crate::use_cases::ports::MaterialPalette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaceDirection {
    Up,
    Down,
    North,
    South,
    East,
    West,
}

impl FaceDirection {
    fn occlusion_bit(self) -> u8 {
        match self {
            FaceDirection::East => FACE_OCCLUDED_POSITIVE_X,
            FaceDirection::West => FACE_OCCLUDED_NEGATIVE_X,
            FaceDirection::Up => FACE_OCCLUDED_POSITIVE_Y,
            FaceDirection::Down => FACE_OCCLUDED_NEGATIVE_Y,
            FaceDirection::South => FACE_OCCLUDED_POSITIVE_Z,
            FaceDirection::North => FACE_OCCLUDED_NEGATIVE_Z,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MergedQuad {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
    pub h: f32,
    pub dir: FaceDirection,
    pub material: u8,
    /// Base material color. Lighting stays separate so renderers can shade
    /// consistently instead of baking a different color per presentation.
    pub color: u32,
    /// Scalar baked voxel light, 0--15.
    pub light: u8,
    /// Directional face occlusion bit for this exposed face (0 or 1).
    pub ao: u8,
}

/// Occupancy lookup used by the surface extractor. Coordinates are relative
/// to the meshed chunk; implementations may resolve a one-voxel halo from
/// adjacent chunks or a deterministically generated border. Missing lateral
/// neighbors must be conservative (solid), never treated as air.
pub trait VoxelNeighborhood {
    fn voxel(&self, x: i32, y: i32, z: i32) -> u8;
}

struct GridNeighborhood<'a> {
    grid: &'a VoxelGrid,
    origin: [usize; 3],
    conservative_lateral_border: bool,
}

impl VoxelNeighborhood for GridNeighborhood<'_> {
    fn voxel(&self, x: i32, y: i32, z: i32) -> u8 {
        let gx = self.origin[0] as i32 + x;
        let gy = self.origin[1] as i32 + y;
        let gz = self.origin[2] as i32 + z;
        if gy < 0 || gy >= self.grid.height() as i32 {
            return VOXEL_AIR;
        }
        if gx < 0 || gx >= self.grid.width() as i32 || gz < 0 || gz >= self.grid.depth() as i32 {
            return if self.conservative_lateral_border {
                VOXEL_WALL
            } else {
                VOXEL_AIR
            };
        }
        self.grid.get(gx as usize, gy as usize, gz as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceMeshingPolicy {
    /// Exact meshing matches material, color, baked light, and ambient occlusion.
    /// Preserves maximum visual fidelity per voxel face at the cost of higher quad/triangle count.
    #[default]
    Exact,
    /// Low-spec meshing matches material, color, and face direction, while ignoring baked light
    /// and ambient occlusion variations during greedy merge. Allows continuous architectural
    /// surfaces to merge into the largest possible rectangular quads (2 triangles each).
    LowSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FaceKey {
    material: u8,
    color: u32,
    light: u8,
    ao: u8,
}

impl FaceKey {
    fn matches(&self, other: &Self, policy: SurfaceMeshingPolicy) -> bool {
        match policy {
            SurfaceMeshingPolicy::Exact => self == other,
            SurfaceMeshingPolicy::LowSpec => {
                self.material == other.material && self.color == other.color
            }
        }
    }
}

pub struct VoxelMapper<'a> {
    pub voxel_scale: f32,
    palette: &'a dyn MaterialPalette,
    pub policy: SurfaceMeshingPolicy,
}

impl<'a> VoxelMapper<'a> {
    pub fn new(voxel_scale: f32, palette: &'a dyn MaterialPalette) -> Self {
        Self {
            voxel_scale,
            palette,
            policy: SurfaceMeshingPolicy::Exact,
        }
    }

    pub fn with_policy(
        voxel_scale: f32,
        palette: &'a dyn MaterialPalette,
        policy: SurfaceMeshingPolicy,
    ) -> Self {
        Self {
            voxel_scale,
            palette,
            policy,
        }
    }

    /// Maps the entire grid into merged exposed quads using default Exact policy.
    pub fn map_voxel_grid(&self, grid: &VoxelGrid) -> Vec<MergedQuad> {
        self.map_voxel_grid_with_policy(grid, self.policy)
    }

    /// Maps the entire grid into merged exposed quads with a specific meshing policy.
    pub fn map_voxel_grid_with_policy(
        &self,
        grid: &VoxelGrid,
        policy: SurfaceMeshingPolicy,
    ) -> Vec<MergedQuad> {
        let neighborhood = GridNeighborhood {
            grid,
            origin: [0, 0, 0],
            conservative_lateral_border: false,
        };
        self.map_voxel_region_with_policy(
            grid,
            [0, 0, 0],
            [grid.width(), grid.height(), grid.depth()],
            &neighborhood,
            policy,
        )
    }

    /// Greedily meshes the interior of a grid carrying a one-voxel X/Z halo.
    pub fn map_voxel_grid_with_padding(
        &self,
        grid: &VoxelGrid,
        lateral_padding: usize,
    ) -> Vec<MergedQuad> {
        self.map_voxel_grid_with_padding_and_policy(grid, lateral_padding, self.policy)
    }

    /// Greedily meshes with padding and an explicit surface meshing policy.
    pub fn map_voxel_grid_with_padding_and_policy(
        &self,
        grid: &VoxelGrid,
        lateral_padding: usize,
        policy: SurfaceMeshingPolicy,
    ) -> Vec<MergedQuad> {
        assert!(lateral_padding > 0, "surface meshing requires a voxel halo");
        assert!(grid.width() > lateral_padding * 2 && grid.depth() > lateral_padding * 2);
        let neighborhood = GridNeighborhood {
            grid,
            origin: [lateral_padding, 0, lateral_padding],
            conservative_lateral_border: true,
        };
        self.map_voxel_region_with_policy(
            grid,
            [lateral_padding, 0, lateral_padding],
            [
                grid.width() - lateral_padding * 2,
                grid.height(),
                grid.depth() - lateral_padding * 2,
            ],
            &neighborhood,
            policy,
        )
    }

    /// Surface extraction against a supplied occupancy neighborhood.
    pub fn map_voxel_region(
        &self,
        grid: &VoxelGrid,
        interior_origin: [usize; 3],
        dimensions: [usize; 3],
        neighborhood: &dyn VoxelNeighborhood,
    ) -> Vec<MergedQuad> {
        self.map_voxel_region_with_policy(
            grid,
            interior_origin,
            dimensions,
            neighborhood,
            self.policy,
        )
    }

    /// Surface extraction against a supplied occupancy neighborhood with explicit policy.
    pub fn map_voxel_region_with_policy(
        &self,
        grid: &VoxelGrid,
        interior_origin: [usize; 3],
        dimensions: [usize; 3],
        neighborhood: &dyn VoxelNeighborhood,
        policy: SurfaceMeshingPolicy,
    ) -> Vec<MergedQuad> {
        let mut quads = Vec::new();
        let scale = self.voxel_scale;
        let [w_dim, h_dim, d_dim] = dimensions;

        let attrs = |x: usize, y: usize, z: usize, v: u8, dir: FaceDirection| {
            self.face_key(
                grid,
                interior_origin[0] + x,
                interior_origin[1] + y,
                interior_origin[2] + z,
                v,
                dir,
            )
        };
        let mut append = |x: f32, y: f32, z: f32, w: f32, h: f32, dir, key: FaceKey| {
            quads.push(MergedQuad {
                x,
                y,
                z,
                w,
                h,
                dir,
                material: key.material,
                color: key.color,
                light: key.light,
                ao: key.ao,
            });
        };

        // +Y / -Y faces: 2D slices in X/Z.
        for y in 0..h_dim {
            for (dir, dy) in [(FaceDirection::Up, 1), (FaceDirection::Down, -1)] {
                let get_face = |x: usize, z: usize| -> Option<FaceKey> {
                    let v = neighborhood.voxel(x as i32, y as i32, z as i32);
                    (v != VOXEL_AIR
                        && neighborhood.voxel(x as i32, y as i32 + dy, z as i32) == VOXEL_AIR)
                        .then(|| attrs(x, y, z, v, dir))
                };
                for (u, v, qw, qh, key) in self.greedy_mesh_2d(w_dim, d_dim, &get_face, policy) {
                    append(
                        u as f32 * scale,
                        y as f32 * scale,
                        v as f32 * scale,
                        qw as f32 * scale,
                        qh as f32 * scale,
                        dir,
                        key,
                    );
                }
            }
        }

        // -Z / +Z faces: 2D slices in X/Y.
        for z in 0..d_dim {
            for (dir, dz) in [(FaceDirection::North, -1), (FaceDirection::South, 1)] {
                let get_face = |x: usize, y: usize| -> Option<FaceKey> {
                    let v = neighborhood.voxel(x as i32, y as i32, z as i32);
                    (v != VOXEL_AIR
                        && neighborhood.voxel(x as i32, y as i32, z as i32 + dz) == VOXEL_AIR)
                        .then(|| attrs(x, y, z, v, dir))
                };
                for (u, v, qw, qh, key) in self.greedy_mesh_2d(w_dim, h_dim, &get_face, policy) {
                    append(
                        u as f32 * scale,
                        v as f32 * scale,
                        z as f32 * scale,
                        qw as f32 * scale,
                        qh as f32 * scale,
                        dir,
                        key,
                    );
                }
            }
        }

        // +X / -X faces: 2D slices in Z/Y.
        for x in 0..w_dim {
            for (dir, dx) in [(FaceDirection::East, 1), (FaceDirection::West, -1)] {
                let get_face = |z: usize, y: usize| -> Option<FaceKey> {
                    let v = neighborhood.voxel(x as i32, y as i32, z as i32);
                    (v != VOXEL_AIR
                        && neighborhood.voxel(x as i32 + dx, y as i32, z as i32) == VOXEL_AIR)
                        .then(|| attrs(x, y, z, v, dir))
                };
                for (u, v, qw, qh, key) in self.greedy_mesh_2d(d_dim, h_dim, &get_face, policy) {
                    append(
                        x as f32 * scale,
                        v as f32 * scale,
                        u as f32 * scale,
                        qw as f32 * scale,
                        qh as f32 * scale,
                        dir,
                        key,
                    );
                }
            }
        }

        quads
    }

    fn face_key(
        &self,
        grid: &VoxelGrid,
        x: usize,
        y: usize,
        z: usize,
        voxel: u8,
        dir: FaceDirection,
    ) -> FaceKey {
        let ao = u8::from(grid.get_face_occlusion(x, y, z) & dir.occlusion_bit() != 0);
        FaceKey {
            material: voxel,
            color: self.palette.color(voxel),
            light: grid.get_light(x, y, z),
            ao,
        }
    }

    /// Helper that performs 2D greedy meshing on a slice with a specified meshing policy.
    fn greedy_mesh_2d(
        &self,
        w_slice: usize,
        h_slice: usize,
        get_face: &dyn Fn(usize, usize) -> Option<FaceKey>,
        policy: SurfaceMeshingPolicy,
    ) -> Vec<(usize, usize, usize, usize, FaceKey)> {
        let mut visited = vec![vec![false; h_slice]; w_slice];
        let mut quads = Vec::new();

        for v in 0..h_slice {
            for u in 0..w_slice {
                if visited[u][v] {
                    continue;
                }

                if let Some(key) = get_face(u, v) {
                    // Find max width along u
                    let mut quad_w = 1;
                    while u + quad_w < w_slice {
                        if visited[u + quad_w][v] {
                            break;
                        }
                        if let Some(next) = get_face(u + quad_w, v)
                            && key.matches(&next, policy)
                        {
                            quad_w += 1;
                            continue;
                        }
                        break;
                    }

                    // Find max height along v
                    let mut quad_h = 1;
                    'expand_h: while v + quad_h < h_slice {
                        for du in 0..quad_w {
                            if visited[u + du][v + quad_h] {
                                break 'expand_h;
                            }
                            if let Some(next) = get_face(u + du, v + quad_h) {
                                if !key.matches(&next, policy) {
                                    break 'expand_h;
                                }
                            } else {
                                break 'expand_h;
                            }
                        }
                        quad_h += 1;
                    }

                    // Mark matched area as visited
                    for dv in 0..quad_h {
                        for du in 0..quad_w {
                            visited[u + du][v + dv] = true;
                        }
                    }

                    quads.push((u, v, quad_w, quad_h, key));
                }
            }
        }
        quads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;

    #[test]
    fn test_voxel_mapping() {
        let mut grid = VoxelGrid::new(2, 2, 2);
        grid.set(0, 0, 0, VOXEL_WALL);

        let mapper = VoxelMapper::new(1.0, &DEFAULT_MATERIAL_PALETTE);
        let quads = mapper.map_voxel_grid(&grid);

        // Single isolated wall block should have 6 faces merged as 6 quads of size 1x1
        assert_eq!(quads.len(), 6);
        for q in quads {
            assert_eq!(q.w, 1.0);
            assert_eq!(q.h, 1.0);
        }
    }

    #[test]
    fn test_greedy_merging() {
        let mut grid = VoxelGrid::new(4, 1, 1);
        grid.set(0, 0, 0, VOXEL_WALL);
        grid.set(1, 0, 0, VOXEL_WALL);
        grid.set(2, 0, 0, VOXEL_WALL);
        grid.set(3, 0, 0, VOXEL_WALL);

        let mapper = VoxelMapper::new(1.0, &DEFAULT_MATERIAL_PALETTE);
        let quads = mapper.map_voxel_grid(&grid);

        // The Up faces of these 4 aligned blocks should merge into a single 4x1 quad!
        let up_quads: Vec<&MergedQuad> = quads
            .iter()
            .filter(|q| q.dir == FaceDirection::Up)
            .collect();
        assert_eq!(up_quads.len(), 1);
        assert_eq!(up_quads[0].w, 4.0);
        assert_eq!(up_quads[0].h, 1.0);
    }

    #[test]
    fn padded_mesher_does_not_emit_face_against_solid_halo() {
        // X/Z padding is one cell. The rightmost interior voxel touches a
        // solid halo voxel, so its +X face must not be generated.
        let mut grid = VoxelGrid::new(4, 1, 3);
        grid.set(2, 0, 1, VOXEL_WALL); // interior x = 1
        grid.set(3, 0, 1, VOXEL_WALL); // +X halo

        let mapper = VoxelMapper::new(1.0, &DEFAULT_MATERIAL_PALETTE);
        let quads = mapper.map_voxel_grid_with_padding(&grid, 1);
        assert!(
            !quads.iter().any(|q| q.dir == FaceDirection::East),
            "solid neighbor halo must suppress the boundary face"
        );
    }

    #[test]
    fn greedy_merge_respects_baked_light_and_ao() {
        let mut grid = VoxelGrid::new(2, 1, 1);
        grid.set(0, 0, 0, VOXEL_WALL);
        grid.set(1, 0, 0, VOXEL_WALL);
        grid.set_light(0, 0, 0, 5);
        grid.set_light(1, 0, 0, 6);

        let mapper = VoxelMapper::new(1.0, &DEFAULT_MATERIAL_PALETTE);
        let up: Vec<_> = mapper
            .map_voxel_grid(&grid)
            .into_iter()
            .filter(|q| q.dir == FaceDirection::Up)
            .collect();
        assert_eq!(up.len(), 2, "different baked light must split a quad");
    }

    #[test]
    fn low_spec_policy_merges_faces_with_different_baked_light() {
        let mut grid = VoxelGrid::new(2, 1, 1);
        grid.set(0, 0, 0, VOXEL_WALL);
        grid.set(1, 0, 0, VOXEL_WALL);
        grid.set_light(0, 0, 0, 5);
        grid.set_light(1, 0, 0, 12);

        let mapper = VoxelMapper::with_policy(
            1.0,
            &DEFAULT_MATERIAL_PALETTE,
            SurfaceMeshingPolicy::LowSpec,
        );
        let up: Vec<_> = mapper
            .map_voxel_grid(&grid)
            .into_iter()
            .filter(|q| q.dir == FaceDirection::Up)
            .collect();
        assert_eq!(
            up.len(),
            1,
            "low-spec policy must merge adjacent faces despite light differences"
        );
        assert_eq!(up[0].w, 2.0);
    }

    #[test]
    fn exact_policy_splits_on_different_ao() {
        let mut grid = VoxelGrid::new(2, 1, 1);
        grid.set(0, 0, 0, VOXEL_WALL);
        grid.set(1, 0, 0, VOXEL_WALL);
        // Simulate ambient occlusion difference by setting face occlusion bits
        grid.set_face_occlusion(0, 0, 0, 0b11111111);
        grid.set_face_occlusion(1, 0, 0, 0);

        let mapper_exact =
            VoxelMapper::with_policy(1.0, &DEFAULT_MATERIAL_PALETTE, SurfaceMeshingPolicy::Exact);
        let up_exact: Vec<_> = mapper_exact
            .map_voxel_grid(&grid)
            .into_iter()
            .filter(|q| q.dir == FaceDirection::Up)
            .collect();
        assert_eq!(
            up_exact.len(),
            2,
            "exact policy must split quads on AO difference"
        );

        let mapper_low = VoxelMapper::with_policy(
            1.0,
            &DEFAULT_MATERIAL_PALETTE,
            SurfaceMeshingPolicy::LowSpec,
        );
        let up_low: Vec<_> = mapper_low
            .map_voxel_grid(&grid)
            .into_iter()
            .filter(|q| q.dir == FaceDirection::Up)
            .collect();
        assert_eq!(
            up_low.len(),
            1,
            "low-spec policy must merge quads despite AO difference"
        );
    }

    #[test]
    fn different_materials_never_merge_in_either_policy() {
        use crate::domain::entities::voxel_grid::VOXEL_FLOOR;

        let mut grid = VoxelGrid::new(2, 1, 1);
        grid.set(0, 0, 0, VOXEL_WALL);
        grid.set(1, 0, 0, VOXEL_FLOOR);

        for policy in [SurfaceMeshingPolicy::Exact, SurfaceMeshingPolicy::LowSpec] {
            let mapper = VoxelMapper::with_policy(1.0, &DEFAULT_MATERIAL_PALETTE, policy);
            let up: Vec<_> = mapper
                .map_voxel_grid(&grid)
                .into_iter()
                .filter(|q| q.dir == FaceDirection::Up)
                .collect();
            assert_eq!(
                up.len(),
                2,
                "different materials must never merge in {:?}",
                policy
            );
        }
    }

    #[test]
    fn different_face_directions_never_merge() {
        let mut grid = VoxelGrid::new(1, 1, 1);
        grid.set(0, 0, 0, VOXEL_WALL);

        let mapper = VoxelMapper::with_policy(
            1.0,
            &DEFAULT_MATERIAL_PALETTE,
            SurfaceMeshingPolicy::LowSpec,
        );
        let quads = mapper.map_voxel_grid(&grid);
        assert_eq!(quads.len(), 6, "single cube has 6 distinct face directions");
    }

    #[test]
    fn solid_halo_suppresses_boundary_face_in_both_modes() {
        let mut grid = VoxelGrid::new(4, 1, 3);
        grid.set(2, 0, 1, VOXEL_WALL); // interior x = 1
        grid.set(3, 0, 1, VOXEL_WALL); // +X halo

        for policy in [SurfaceMeshingPolicy::Exact, SurfaceMeshingPolicy::LowSpec] {
            let mapper = VoxelMapper::with_policy(1.0, &DEFAULT_MATERIAL_PALETTE, policy);
            let quads = mapper.map_voxel_grid_with_padding(&grid, 1);
            assert!(
                !quads.iter().any(|q| q.dir == FaceDirection::East),
                "solid neighbor halo must suppress the boundary face in {:?}",
                policy
            );
        }
    }

    #[test]
    fn uniform_rectangular_floor_creates_minimal_quads() {
        let mut grid = VoxelGrid::new(4, 1, 4);
        for x in 0..4 {
            for z in 0..4 {
                grid.set(x, 0, z, VOXEL_WALL);
                grid.set_light(x, 0, z, ((x + z) % 15) as u8); // Varying light
            }
        }

        let mapper = VoxelMapper::with_policy(
            1.0,
            &DEFAULT_MATERIAL_PALETTE,
            SurfaceMeshingPolicy::LowSpec,
        );
        let up: Vec<_> = mapper
            .map_voxel_grid(&grid)
            .into_iter()
            .filter(|q| q.dir == FaceDirection::Up)
            .collect();
        assert_eq!(
            up.len(),
            1,
            "a uniform 4x4 floor with varying light must merge into 1 quad in low-spec mode"
        );
        assert_eq!(up[0].w, 4.0);
        assert_eq!(up[0].h, 4.0);
    }

    #[test]
    fn emissive_and_non_emissive_materials_do_not_merge() {
        use crate::domain::entities::voxel_grid::VOXEL_LIGHT;

        let mut grid = VoxelGrid::new(2, 1, 1);
        grid.set(0, 0, 0, VOXEL_WALL);
        grid.set(1, 0, 0, VOXEL_LIGHT); // Emissive material

        let mapper = VoxelMapper::with_policy(
            1.0,
            &DEFAULT_MATERIAL_PALETTE,
            SurfaceMeshingPolicy::LowSpec,
        );
        let up: Vec<_> = mapper
            .map_voxel_grid(&grid)
            .into_iter()
            .filter(|q| q.dir == FaceDirection::Up)
            .collect();
        assert_eq!(
            up.len(),
            2,
            "emissive fixture face must not merge with regular wall face"
        );
    }
}
