//! Converts the core greedy-quad output into the browser renderer's compact
//! indexed mesh payload. Geometry stays chunk-local and fixed-point so GPU
//! uploads are small and camera precision does not degrade far from origin.

use vackrooms::adapters::material_palette::DEFAULT_MATERIAL_PALETTE;
use vackrooms::adapters::voxel_mapper::{FaceDirection, MergedQuad, VoxelMapper};
use vackrooms::domain::entities::voxel_grid::VoxelGrid;

use vackrooms::domain::entities::probe_grid::{
    DEFAULT_PROBE_DEPTH, DEFAULT_PROBE_HEIGHT, DEFAULT_PROBE_WIDTH, ProbeGrid,
};

use crate::application::collision::Aabb;
use crate::application::ports::{
    POSITION_FIXED_SCALE, PackedVertex, RenderArtifactNeeds, SurfaceMeshPayload,
};

/// Builds a seam-safe surface payload from a grid that includes a one-voxel
/// X/Z halo. The mapper consults that halo before emitting boundary faces.
pub fn build_surface_mesh(
    halo_grid: &VoxelGrid,
    voxel_scale: f32,
    lod: u8,
    lateral_padding: usize,
) -> SurfaceMeshPayload {
    build_surface_artifacts(
        halo_grid,
        voxel_scale,
        lod,
        lateral_padding,
        RenderArtifactNeeds::SURFACE,
        SurfelDensity::PER_VOXEL,
    )
}

/// How finely a surfel cloud samples the surface, as a multiple of the
/// per-voxel default.
///
/// `1.0` puts one disc in each voxel cell at this LOD — the density that
/// makes a cloud interchangeable with the mesh rather than a coarser stand-in
/// for it. Higher values subdivide further: `2.0` halves the spacing, so
/// four times the discs at half the radius each.
///
/// Radius follows spacing (`radius_for_spacing` is `spacing · √2/2`), so
/// raising density shrinks every disc *and* keeps coverage hole-free. That is
/// what makes it the honest dial for the scalloped silhouette where discs
/// overhang a surface boundary: the overhang is one radius wide, so it
/// shrinks in proportion while the surface stays closed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfelDensity(f32);

impl SurfelDensity {
    /// One disc per voxel cell.
    pub const PER_VOXEL: Self = Self(1.0);

    pub fn new(density: f32) -> Self {
        // Clamped rather than rejected: this arrives from a URL. The ceiling
        // is where `build_surfel_cloud` stops honoring spacing anyway (it
        // floors at `voxel_scale * 0.1`), and the floor keeps a cloud from
        // degenerating into one disc per merged quad.
        Self(density.clamp(0.25, 8.0))
    }

    pub fn get(self) -> f32 {
        self.0
    }

    /// The spacing this density asks for at a given voxel scale.
    pub fn spacing(self, voxel_scale: f32) -> f32 {
        voxel_scale / self.0
    }
}

impl Default for SurfelDensity {
    fn default() -> Self {
        Self::PER_VOXEL
    }
}

/// Builds only the raster representation selected by the renderer. Greedy
/// quads are shared by both outputs, but indexed vertices and face instances
/// are materialized independently so neither strategy pays for the other.
pub fn build_surface_artifacts(
    halo_grid: &VoxelGrid,
    voxel_scale: f32,
    lod: u8,
    lateral_padding: usize,
    artifacts: RenderArtifactNeeds,
    surfel_density: SurfelDensity,
) -> SurfaceMeshPayload {
    let mapper = VoxelMapper::new(voxel_scale, &DEFAULT_MATERIAL_PALETTE);
    let quads = mapper.map_voxel_grid_with_padding(halo_grid, lateral_padding);
    let width = halo_grid.width() - lateral_padding * 2;
    let depth = halo_grid.depth() - lateral_padding * 2;
    let bounds = Aabb::new(
        [0.0, 0.0, 0.0],
        [
            width as f32 * voxel_scale,
            halo_grid.height() as f32 * voxel_scale,
            depth as f32 * voxel_scale,
        ],
    );
    let probe_grid = ProbeGrid::from_voxel_grid(
        halo_grid,
        DEFAULT_PROBE_WIDTH,
        DEFAULT_PROBE_HEIGHT,
        DEFAULT_PROBE_DEPTH,
    );
    let (pw, ph, pd) = probe_grid.dimensions();
    let mut mesh = SurfaceMeshPayload {
        vertices: Vec::with_capacity(if artifacts.indexed_surface_mesh() {
            quads.len() * 4
        } else {
            0
        }),
        indices: Vec::with_capacity(if artifacts.indexed_surface_mesh() {
            quads.len() * 6
        } else {
            0
        }),
        bounds,
        lod,
        faces: if artifacts.face_splats() {
            crate::adapters::face_instances::build_face_instances(&quads, voxel_scale)
        } else {
            crate::application::ports::FaceInstanceSet::empty()
        },
        // One disc per voxel cell at this LOD by default: the density that
        // makes a surfel cloud interchangeable with the mesh rather than a
        // coarser stand-in for it. `SurfelDensity` subdivides below that;
        // coarser clouds remain a streaming decision, made where the LOD is
        // chosen, not baked in at extraction.
        surfels: if artifacts.surfels() {
            crate::adapters::surfel_cloud::build_surfel_cloud(
                &quads,
                voxel_scale,
                surfel_density.spacing(voxel_scale),
            )
        } else {
            crate::adapters::surfel_cloud::SurfelCloud::empty()
        },
        voxel_scale,
        light_volume_bytes: probe_grid.as_bytes().to_vec(),
        light_volume_dims: [pw as u32, ph as u32, pd as u32],
    };
    if artifacts.indexed_surface_mesh() {
        for quad in &quads {
            append_quad(&mut mesh, quad, voxel_scale);
        }
    }
    mesh
}

fn append_quad(mesh: &mut SurfaceMeshPayload, quad: &MergedQuad, voxel_scale: f32) {
    let x0 = quad.x;
    let x1 = quad.x + quad.w;
    let y0 = quad.y;
    let y1 = quad.y + quad.h;
    let z0 = quad.z;
    let z1_horizontal = quad.z + quad.h;
    let z1_x_face = quad.z + quad.w;

    // `w` and `h` span different local axes per face. Move the positive
    // faces to the far voxel plane so the indexed mesh is a closed shell.
    let points = match quad.dir {
        FaceDirection::Up => {
            let y = quad.y + voxel_scale;
            [
                [x0, y, z0],
                [x0, y, z1_horizontal],
                [x1, y, z1_horizontal],
                [x1, y, z0],
            ]
        }
        FaceDirection::Down => [
            [x0, y0, z0],
            [x1, y0, z0],
            [x1, y0, z1_horizontal],
            [x0, y0, z1_horizontal],
        ],
        FaceDirection::North => [
            [x0, y0, quad.z],
            [x0, y1, quad.z],
            [x1, y1, quad.z],
            [x1, y0, quad.z],
        ],
        FaceDirection::South => {
            let z = quad.z + voxel_scale;
            [[x0, y0, z], [x1, y0, z], [x1, y1, z], [x0, y1, z]]
        }
        FaceDirection::East => {
            let x = quad.x + voxel_scale;
            [
                [x, y0, z0],
                [x, y1, z0],
                [x, y1, z1_x_face],
                [x, y0, z1_x_face],
            ]
        }
        FaceDirection::West => [
            [quad.x, y0, z0],
            [quad.x, y0, z1_x_face],
            [quad.x, y1, z1_x_face],
            [quad.x, y1, z0],
        ],
    };

    let base = mesh.vertices.len() as u32;
    let normal_axis = normal_axis(quad.dir);
    let material = quad.material;
    for position in points {
        mesh.vertices.push(PackedVertex {
            position: position.map(pack_position),
            normal_axis,
            material,
            static_indirect: quad.light,
            ao: quad.ao,
        });
    }
    mesh.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn pack_position(value: f32) -> u16 {
    (value * POSITION_FIXED_SCALE)
        .round()
        .clamp(0.0, u16::MAX as f32) as u16
}

fn normal_axis(dir: FaceDirection) -> u8 {
    match dir {
        FaceDirection::Up => 0,
        FaceDirection::Down => 1,
        FaceDirection::North => 2,
        FaceDirection::South => 3,
        FaceDirection::East => 4,
        FaceDirection::West => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vackrooms::domain::entities::voxel_grid::VOXEL_WALL;

    /// Density subdivides; it must not move the surface. A denser cloud that
    /// also shifted would look like an improvement and be a regression.
    #[test]
    fn raising_density_adds_discs_without_moving_the_surface() {
        let mut grid = VoxelGrid::new(6, 3, 6);
        for x in 0..6 {
            for z in 0..6 {
                grid.set(x, 0, z, VOXEL_WALL);
            }
        }

        let coarse = build_surface_artifacts(
            &grid,
            0.2,
            0,
            1,
            RenderArtifactNeeds::SURFEL,
            SurfelDensity::PER_VOXEL,
        );
        let fine = build_surface_artifacts(
            &grid,
            0.2,
            0,
            1,
            RenderArtifactNeeds::SURFEL,
            SurfelDensity::new(2.0),
        );

        assert!(
            fine.surfels.surfels.len() > coarse.surfels.surfels.len(),
            "halving the spacing must produce more discs, not the same cloud"
        );
        assert_eq!(
            fine.bounds, coarse.bounds,
            "density is a sampling rate, not a change of geometry"
        );

        // Radius follows spacing, so the discs must get smaller in step --
        // that is what keeps coverage hole-free while the silhouette
        // overhang shrinks.
        let widest = |payload: &SurfaceMeshPayload| {
            payload
                .surfels
                .surfels
                .iter()
                .map(|surfel| surfel.radius)
                .max()
                .unwrap_or(0)
        };
        assert!(
            widest(&fine) < widest(&coarse),
            "denser sampling must shrink the discs, not just add them"
        );
    }

    /// The density arrives from a URL, so it is clamped rather than trusted.
    #[test]
    fn an_absurd_density_is_clamped_not_obeyed() {
        assert_eq!(SurfelDensity::new(0.0).get(), 0.25);
        assert_eq!(SurfelDensity::new(-4.0).get(), 0.25);
        assert_eq!(SurfelDensity::new(1e9).get(), 8.0);
        assert_eq!(SurfelDensity::default(), SurfelDensity::PER_VOXEL);
        assert_eq!(SurfelDensity::new(2.0).spacing(0.4), 0.2);
    }

    #[test]
    fn isolated_voxel_becomes_indexed_closed_surface() {
        let mut grid = VoxelGrid::new(3, 2, 3);
        grid.set(1, 0, 1, VOXEL_WALL);
        let mesh = build_surface_mesh(&grid, 1.0, 0, 1);
        assert_eq!(mesh.vertices.len(), 24);
        assert_eq!(mesh.indices.len(), 36);
        assert!(mesh.faces.instances.is_empty());
    }

    #[test]
    fn splat_artifacts_do_not_materialize_the_indexed_mesh() {
        let mut grid = VoxelGrid::new(4, 2, 4);
        grid.set(1, 0, 1, VOXEL_WALL);
        let artifacts = build_surface_artifacts(
            &grid,
            1.0,
            0,
            1,
            RenderArtifactNeeds::SPLAT,
            SurfelDensity::PER_VOXEL,
        );

        assert!(artifacts.vertices.is_empty());
        assert!(artifacts.indices.is_empty());
        assert!(!artifacts.faces.instances.is_empty());
    }
}
