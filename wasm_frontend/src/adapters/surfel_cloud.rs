//! Surface elements: the world as a cloud of oriented discs.
//!
//! Every other raster path in this engine draws *the surface's topology* —
//! an indexed mesh of merged quads, or one instanced rectangle per merged
//! quad. Both are locked to the greedy mesher's output: to draw less, you
//! must re-mesh at a coarser voxel scale, which changes what the geometry
//! *is*.
//!
//! A surfel cloud separates those two things. The surface is sampled into
//! discs at a chosen spacing, and spacing is a dial rather than a rebuild:
//! the same chunk yields a dense cloud up close and a sparse one at range
//! from the identical quads, with no re-meshing and no popping between
//! discrete LOD steps. That is the property this representation is for.
//!
//! ## Why the discs overlap
//!
//! A surfel is a disc, and discs laid on a square grid leave gaps at the
//! corners unless each one reaches the corner of its own cell. So the
//! radius is the cell's half-diagonal, not its half-width — see
//! [`radius_for_spacing`]. Getting this wrong does not look like an error;
//! it looks like a faint pinhole texture over every wall, which is exactly
//! the sort of thing that gets mistaken for an intentional grain.
//!
//! ## What is deliberately not here
//!
//! No irradiance. Surfels are also the standard vehicle for a persistent
//! indirect-light cache, and that is a plausible next use for this module,
//! but nothing here reserves space for it: a surfel is 12 bytes of geometry
//! and material today. Lighting, when it comes, belongs in a parallel array
//! indexed by surfel — reserving unused bytes per surfel now would cost
//! real memory across millions of them in exchange for a guess about what
//! that lighting will need.
//!
//! Like the rest of the adapters layer this is platform-free and natively
//! unit-tested.

use vackrooms::adapters::voxel_mapper::{FaceDirection, MergedQuad};

use crate::application::ports::POSITION_FIXED_SCALE;

/// Quantization of a surfel radius, in world units per step.
///
/// A `u8` of these covers 0 to 8 u, which spans everything from a
/// sub-voxel disc to one that swallows a whole room. Radius is stored per
/// surfel rather than derived from the cloud's spacing so that an adaptive
/// cloud — coarse on a far wall, fine on the pier in front of you — is a
/// change of producer, not a change of format.
pub const RADIUS_QUANT: f32 = 1.0 / 32.0;

/// Bit 0 of [`PackedSurfel::flags`]: the material emits light.
pub const SURFEL_FLAG_EMISSIVE: u8 = 1;

/// One oriented surface disc. 12 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedSurfel {
    /// Disc centre, chunk-local fixed point ([`POSITION_FIXED_SCALE`]).
    pub position: [u16; 3],
    /// Radius in [`RADIUS_QUANT`] steps.
    pub radius: u8,
    /// Same encoding as `PackedVertex::normal_axis`.
    pub normal_axis: u8,
    pub material: u8,
    /// Scalar baked voxel light, 0–15.
    pub baked_light: u8,
    /// Directional face occlusion bit (0 or 1).
    pub ao: u8,
    pub flags: u8,
}

impl PackedSurfel {
    pub fn world_position(&self) -> [f32; 3] {
        [
            self.position[0] as f32 / POSITION_FIXED_SCALE,
            self.position[1] as f32 / POSITION_FIXED_SCALE,
            self.position[2] as f32 / POSITION_FIXED_SCALE,
        ]
    }

    pub fn world_radius(&self) -> f32 {
        self.radius as f32 * RADIUS_QUANT
    }

    /// Outward unit normal, decoded from the axis code.
    pub fn normal(&self) -> [f32; 3] {
        match self.normal_axis {
            0 => [0.0, 1.0, 0.0],  // Up
            1 => [0.0, -1.0, 0.0], // Down
            2 => [0.0, 0.0, -1.0], // North
            3 => [0.0, 0.0, 1.0],  // South
            4 => [1.0, 0.0, 0.0],  // East
            _ => [-1.0, 0.0, 0.0], // West
        }
    }
}

/// One chunk's surfels, plus the spacing they were sampled at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfelCloud {
    pub surfels: Vec<PackedSurfel>,
    /// Distance between adjacent disc centres, world units. Recorded so a
    /// consumer can tell a deliberately sparse cloud from a small one.
    pub spacing_millis: u32,
}

impl SurfelCloud {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn spacing(&self) -> f32 {
        self.spacing_millis as f32 / 1000.0
    }

    pub fn is_empty(&self) -> bool {
        self.surfels.is_empty()
    }
}

/// The radius a disc needs to cover its own cell on a square grid.
///
/// Half the cell diagonal. Anything less leaves the four corners of every
/// cell uncovered, which reads as a pinhole grain rather than as a hole.
pub fn radius_for_spacing(spacing: f32) -> f32 {
    spacing * std::f32::consts::SQRT_2 * 0.5
}

/// Samples greedy-merged quads into a surfel cloud at the given spacing.
///
/// `spacing` is in world units and is clamped to something sane: a spacing
/// finer than a tenth of a voxel would produce hundreds of surfels per
/// voxel face and no more detail, because the quads carry no detail below
/// one voxel.
pub fn build_surfel_cloud(quads: &[MergedQuad], voxel_scale: f32, spacing: f32) -> SurfelCloud {
    let spacing = spacing.clamp(voxel_scale * 0.1, 64.0);
    let radius = radius_for_spacing(spacing);
    let radius_q = ((radius / RADIUS_QUANT).round() as u32).clamp(1, u8::MAX as u32) as u8;

    let mut surfels = Vec::new();
    for quad in quads {
        emit_quad(quad, voxel_scale, spacing, radius_q, &mut surfels);
    }
    SurfelCloud {
        surfels,
        spacing_millis: (spacing * 1000.0).round() as u32,
    }
}

/// Scatters one quad's discs.
///
/// The quad's own extent decides the count, so a merged 10 u wall and the
/// forty separate faces it was merged from produce the same cloud. That
/// equivalence is what lets the cloud be sampled from merged geometry
/// without inheriting the mesher's arbitrary merge boundaries.
fn emit_quad(
    quad: &MergedQuad,
    voxel_scale: f32,
    spacing: f32,
    radius_q: u8,
    out: &mut Vec<PackedSurfel>,
) {
    // `w` and `h` are already world extents, not voxel counts -- the same
    // convention `append_quad` reads them under.
    let (u_len, v_len) = (quad.w, quad.h);
    // At least one disc per quad: a face smaller than the spacing still
    // exists, and dropping it would punch a hole in the surface that no
    // amount of neighbouring coverage fills.
    let u_count = (u_len / spacing).round().max(1.0) as u32;
    let v_count = (v_len / spacing).round().max(1.0) as u32;
    let (u_step, v_step) = (u_len / u_count as f32, v_len / v_count as f32);

    let normal_axis = normal_axis_of(quad.dir);
    let flags = if is_emissive(quad.material) {
        SURFEL_FLAG_EMISSIVE
    } else {
        0
    };

    for iv in 0..v_count {
        for iu in 0..u_count {
            // Centre of the cell, not its corner: a disc centred on the
            // quad's edge would hang half its area off the surface.
            let u = (iu as f32 + 0.5) * u_step;
            let v = (iv as f32 + 0.5) * v_step;
            let world = quad_point(quad, voxel_scale, u, v);
            let Some(position) = pack_position(world) else {
                continue;
            };
            out.push(PackedSurfel {
                position,
                radius: radius_q,
                normal_axis,
                material: quad.material,
                baked_light: quad.light.min(15),
                ao: quad.ao.min(1),
                flags,
            });
        }
    }
}

/// A point on the quad's plane, `u` and `v` world units along its two
/// in-plane axes from its origin corner.
///
/// Which axes `w` and `h` span differs per face, and the positive faces sit
/// one voxel out on their own axis so the shell closes — both conventions
/// are `append_quad`'s, mirrored here rather than reinvented. A surfel
/// cloud that disagreed with the mesh about where a wall is would show up
/// as discs sunk a voxel into every south-facing surface.
fn quad_point(quad: &MergedQuad, voxel_scale: f32, u: f32, v: f32) -> [f32; 3] {
    let (x, y, z) = (quad.x, quad.y, quad.z);
    match quad.dir {
        // Y-facing planes span X (u) and Z (v).
        FaceDirection::Up => [x + u, y + voxel_scale, z + v],
        FaceDirection::Down => [x + u, y, z + v],
        // Z-facing planes span X (u) and Y (v).
        FaceDirection::North => [x + u, y + v, z],
        FaceDirection::South => [x + u, y + v, z + voxel_scale],
        // X-facing planes span Z (u) and Y (v).
        FaceDirection::East => [x + voxel_scale, y + v, z + u],
        FaceDirection::West => [x, y + v, z + u],
    }
}

/// The face's outward axis, in `PackedVertex::normal_axis`'s encoding.
fn normal_axis_of(dir: FaceDirection) -> u8 {
    match dir {
        FaceDirection::Up => 0,
        FaceDirection::Down => 1,
        FaceDirection::North => 2,
        FaceDirection::South => 3,
        FaceDirection::East => 4,
        FaceDirection::West => 5,
    }
}

fn is_emissive(material: u8) -> bool {
    use vackrooms::domain::entities::voxel_grid::{VOXEL_GLIMMER, VOXEL_LIGHT, VOXEL_RED_LIGHT};
    matches!(material, VOXEL_LIGHT | VOXEL_RED_LIGHT | VOXEL_GLIMMER)
}

/// Chunk-local fixed point, or `None` when the point falls outside the
/// representable box. Dropping is correct here: a surfel whose position
/// wrapped would draw a disc somewhere it does not belong, and one missing
/// disc is a far smaller error than one in the wrong place.
fn pack_position(world: [f32; 3]) -> Option<[u16; 3]> {
    let mut packed = [0u16; 3];
    for axis in 0..3 {
        let fixed = (world[axis] * POSITION_FIXED_SCALE).round();
        if !(0.0..=u16::MAX as f32).contains(&fixed) {
            return None;
        }
        packed[axis] = fixed as u16;
    }
    Some(packed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall(w: f32, h: f32) -> MergedQuad {
        MergedQuad {
            x: 1.0,
            y: 0.0,
            z: 2.0,
            w,
            h,
            dir: FaceDirection::North,
            material: 1,
            color: 0x00DD_CC66,
            light: 9,
            ao: 0,
        }
    }

    /// The property the whole representation rests on: density is a dial,
    /// and turning it does not change what the surface is.
    #[test]
    fn spacing_trades_surfel_count_without_moving_the_surface() {
        let quads = [wall(20.0, 10.0)];
        let fine = build_surfel_cloud(&quads, 0.2, 0.2);
        let coarse = build_surfel_cloud(&quads, 0.2, 0.8);
        assert!(
            fine.surfels.len() > coarse.surfels.len() * 8,
            "coarsening 4x should shed ~16x the surfels: {} vs {}",
            fine.surfels.len(),
            coarse.surfels.len()
        );
        // Both clouds sit on the same plane, whatever their density.
        for cloud in [&fine, &coarse] {
            for surfel in &cloud.surfels {
                let z = surfel.world_position()[2];
                assert!((z - 2.0).abs() < 0.01, "surfel left the quad's plane: {z}");
            }
        }
    }

    /// Discs must reach the corners of their own cells or the surface is
    /// covered in pinholes -- the failure that reads as texture, not as a
    /// bug.
    #[test]
    fn discs_cover_the_gaps_between_their_neighbours() {
        let spacing = 0.5f32;
        let radius = radius_for_spacing(spacing);
        // Worst case on a square grid: the point equidistant from four
        // centres, half a diagonal away from each.
        let corner_distance = spacing * std::f32::consts::SQRT_2 * 0.5;
        assert!(
            radius >= corner_distance - 1e-6,
            "radius {radius} cannot reach a cell corner at {corner_distance}"
        );
    }

    /// A merged quad and the faces it was merged from must sample to the
    /// same surface, or the cloud would inherit the mesher's merge seams.
    #[test]
    fn merging_does_not_change_the_cloud() {
        let merged = build_surfel_cloud(&[wall(4.0, 1.0)], 1.0, 1.0);
        let split: Vec<MergedQuad> = (0..4)
            .map(|i| {
                let mut q = wall(1.0, 1.0);
                q.x = 1.0 + i as f32;
                q
            })
            .collect();
        let piecewise = build_surfel_cloud(&split, 1.0, 1.0);
        let mut a: Vec<_> = merged.surfels.iter().map(|s| s.position).collect();
        let mut b: Vec<_> = piecewise.surfels.iter().map(|s| s.position).collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "merged and unmerged geometry sampled differently");
    }

    /// A face smaller than the spacing is still a face. Rounding it away
    /// would open a hole that no neighbour covers.
    #[test]
    fn a_face_smaller_than_the_spacing_still_gets_a_surfel() {
        let cloud = build_surfel_cloud(&[wall(1.0, 1.0)], 0.2, 4.0);
        assert_eq!(cloud.surfels.len(), 1);
    }

    /// Every face direction must sample onto its own plane. A transposed
    /// axis pair here would scatter a wall's discs across the floor, and
    /// the plane test above only checks one orientation.
    #[test]
    fn every_face_direction_samples_its_own_plane() {
        for (dir, fixed_axis, out_by_a_voxel) in [
            (FaceDirection::East, 0, true),
            (FaceDirection::West, 0, false),
            (FaceDirection::Up, 1, true),
            (FaceDirection::Down, 1, false),
            (FaceDirection::South, 2, true),
            (FaceDirection::North, 2, false),
        ] {
            let mut quad = wall(6.0, 6.0);
            quad.dir = dir;
            let voxel_scale = 1.0;
            let expected = [quad.x, quad.y, quad.z][fixed_axis]
                + if out_by_a_voxel { voxel_scale } else { 0.0 };
            let cloud = build_surfel_cloud(&[quad], voxel_scale, 1.0);
            assert!(!cloud.is_empty(), "{dir:?} produced nothing");
            for surfel in &cloud.surfels {
                let got = surfel.world_position()[fixed_axis];
                assert!(
                    (got - expected).abs() < 0.01,
                    "{dir:?} left its plane: {got} vs {expected}"
                );
            }
            // And the normal has to agree with the plane it sits on.
            let normal = cloud.surfels[0].normal();
            assert_eq!(
                normal[fixed_axis].abs(),
                1.0,
                "{dir:?} normal does not face its own axis"
            );
        }
    }

    /// A disc placed outside the representable box is dropped rather than
    /// wrapped. One missing surfel is invisible; one in the wrong place is
    /// a disc floating in a corridor.
    #[test]
    fn a_surfel_outside_the_chunk_box_is_dropped_not_wrapped() {
        let mut quad = wall(1.0, 1.0);
        quad.x = -50.0;
        let cloud = build_surfel_cloud(&[quad], 1.0, 1.0);
        assert!(cloud.is_empty(), "a negative position was packed anyway");
    }

    #[test]
    fn emissive_materials_are_flagged() {
        use vackrooms::domain::entities::voxel_grid::VOXEL_LIGHT;
        let mut quad = wall(1.0, 1.0);
        quad.material = VOXEL_LIGHT;
        let cloud = build_surfel_cloud(&[quad], 1.0, 1.0);
        assert_eq!(cloud.surfels[0].flags & SURFEL_FLAG_EMISSIVE, 1);
    }

    /// Twelve bytes, and it stays twelve: this is per-visible-surface-point
    /// storage, so a field added carelessly costs megabytes a chunk.
    #[test]
    fn a_surfel_stays_twelve_bytes() {
        assert_eq!(std::mem::size_of::<PackedSurfel>(), 12);
    }
}
