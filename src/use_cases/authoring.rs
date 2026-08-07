//! User-authored construction: the CAD sandbox's edit model.
//!
//! The generator answers "what is here"; this answers "what did someone
//! build here". An [`AuthoringSession`] is an ordered list of CAD operations
//! — add this wall, cut that doorway, drop a desk — held as CSG solids and
//! baked into generated chunks. Order is the edit history: a doorway cut
//! after a wall is a hole in it, the same cut before it is nothing.
//!
//! Why solids rather than painted voxels: an authored wall has to survive
//! being re-voxelized at a different LOD, and a doorway has to stay a
//! doorway when it is. Storing the *operation* keeps that true at any voxel
//! size, which painting individual voxels cannot.
//!
//! The rule this module exists to enforce is [`rough_opening`]. Voxelization
//! samples cell centers, so a cut made at exactly the finished width loses
//! up to a voxel of clearance to the jambs and the door comes out too narrow
//! to walk through. Real framing has the same problem and the same answer:
//! frame the rough opening oversize, then finish it. Every cut this module
//! makes goes through that rule, so an authored door is passable by
//! construction rather than by the author having remembered.

use crate::domain::entities::position::Position;
use crate::domain::entities::voxel_grid::{VOXEL_AIR, VoxelGrid};
use crate::domain::use_cases::csg::{Solid, Vec3, v3};

/// What an operation does to the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOp {
    /// Add solid matter in a material.
    Add,
    /// Remove matter, leaving air. Doorways, windows, demolition.
    Cut,
}

/// One authored construction operation.
#[derive(Clone, Debug)]
pub struct AuthoredElement {
    pub solid: Solid,
    pub op: EditOp,
    /// Material for [`EditOp::Add`]; ignored for a cut.
    pub material: u8,
}

/// Enlarges a finished opening dimension to the rough opening that will
/// actually voxelize to it.
///
/// Voxelization asks whether each cell *center* is inside the solid, so an
/// opening cut at exactly its finished width can lose up to a whole voxel of
/// clear width to the jambs — the cut passes between two cell centers and
/// neither is removed. A 0.9 m door quantized down to 0.8 m at a 0.2 m voxel
/// is no longer a door anyone fits through.
///
/// Oversizing by one voxel guarantees the finished width survives
/// quantization at any grid phase. This is exactly what framing a rough
/// opening does in a real building, for a related reason.
pub fn rough_opening(finished: f32, voxel_size: f32) -> f32 {
    finished + voxel_size
}

/// An ordered edit history over the generated world.
#[derive(Clone, Debug, Default)]
pub struct AuthoringSession {
    elements: Vec<AuthoredElement>,
}

impl AuthoringSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn elements(&self) -> &[AuthoredElement] {
        &self.elements
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Undo: drops the most recent operation.
    pub fn undo(&mut self) -> Option<AuthoredElement> {
        self.elements.pop()
    }

    pub fn push(&mut self, element: AuthoredElement) -> &mut Self {
        self.elements.push(element);
        self
    }

    /// Builds a straight wall between two plan points.
    ///
    /// Centered on the line the author drew, like every CAD wall tool: an
    /// author drawing a room's outline means the wall's centerline, and a
    /// wall grown to one side of it puts the room off by half a thickness.
    pub fn wall(
        &mut self,
        from: Position,
        to: Position,
        thickness: f32,
        height: f32,
        material: u8,
    ) -> &mut Self {
        let half = (thickness * 0.5) as f64;
        let (x0, x1) = ordered(from.x as f64, to.x as f64);
        let (z0, z1) = ordered(from.z as f64, to.z as f64);
        // Axis-aligned walls only, matching the rest of the plan vocabulary.
        // The thin axis takes the thickness; a diagonal request degrades to
        // its bounding run rather than silently building nothing.
        let (min, max) = if (x1 - x0) >= (z1 - z0) {
            let mid = (z0 + z1) * 0.5;
            (v3(x0, 0.0, mid - half), v3(x1, height as f64, mid + half))
        } else {
            let mid = (x0 + x1) * 0.5;
            (v3(mid - half, 0.0, z0), v3(mid + half, height as f64, z1))
        };
        self.push(AuthoredElement {
            solid: Solid::cuboid(min, max),
            op: EditOp::Add,
            material,
        })
    }

    /// Cuts a doorway through whatever stands at `center`.
    ///
    /// `finished_width`/`finished_height` are the dimensions the author
    /// wants to walk through; the cut itself is made at the rough opening,
    /// so that is what they get. `depth` must exceed the wall's thickness or
    /// the cut stops inside it.
    pub fn doorway(
        &mut self,
        center: Position,
        finished_width: f32,
        finished_height: f32,
        depth: f32,
        voxel_size: f32,
        along_x: bool,
    ) -> &mut Self {
        let half_w = (rough_opening(finished_width, voxel_size) * 0.5) as f64;
        // Height is oversized at the head only: the floor already bounds the
        // opening below, and lifting the sill would leave a step to trip on.
        let top = rough_opening(finished_height, voxel_size) as f64;
        let half_d = (depth * 0.5) as f64;
        let (cx, cz) = (center.x as f64, center.z as f64);
        let (min, max) = if along_x {
            (
                v3(cx - half_w, 0.0, cz - half_d),
                v3(cx + half_w, top, cz + half_d),
            )
        } else {
            (
                v3(cx - half_d, 0.0, cz - half_w),
                v3(cx + half_d, top, cz + half_w),
            )
        };
        self.push(AuthoredElement {
            solid: Solid::cuboid(min, max),
            op: EditOp::Cut,
            material: VOXEL_AIR,
        })
    }

    /// A horizontal slab: floor, ceiling plane, or a desk top.
    pub fn slab(
        &mut self,
        from: Position,
        to: Position,
        base_units: f32,
        top_units: f32,
        material: u8,
    ) -> &mut Self {
        let (x0, x1) = ordered(from.x as f64, to.x as f64);
        let (z0, z1) = ordered(from.z as f64, to.z as f64);
        self.push(AuthoredElement {
            solid: Solid::cuboid(v3(x0, base_units as f64, z0), v3(x1, top_units as f64, z1)),
            op: EditOp::Add,
            material,
        })
    }

    /// Bakes the edit history into an already-generated chunk.
    ///
    /// `origin` is the chunk's world corner and `voxel_size` its scale, so
    /// authored geometry lands in world space regardless of which chunk is
    /// being written — an authored wall crossing a chunk seam is written
    /// correctly into both, because both ask the same solids the same
    /// world-space question.
    pub fn apply_to_grid(&self, grid: &mut VoxelGrid, origin: Position, voxel_size: f32) {
        for element in &self.elements {
            let Some((lo, hi)) = element.solid.aabb() else {
                continue;
            };
            // Only walk the cells the element could possibly touch.
            let (ix0, ix1) = cell_range(lo.x, hi.x, origin.x as f64, voxel_size, grid.width());
            let (iy0, iy1) = cell_range(lo.y, hi.y, 0.0, voxel_size, grid.height());
            let (iz0, iz1) = cell_range(lo.z, hi.z, origin.z as f64, voxel_size, grid.depth());
            let bsp = element.solid.bsp();
            for y in iy0..iy1 {
                for z in iz0..iz1 {
                    for x in ix0..ix1 {
                        let point = v3(
                            origin.x as f64 + (x as f64 + 0.5) * voxel_size as f64,
                            (y as f64 + 0.5) * voxel_size as f64,
                            origin.z as f64 + (z as f64 + 0.5) * voxel_size as f64,
                        );
                        if !bsp.contains_point(point) {
                            continue;
                        }
                        match element.op {
                            EditOp::Add => grid.set(x, y, z, element.material),
                            EditOp::Cut => grid.set(x, y, z, VOXEL_AIR),
                        }
                    }
                }
            }
        }
    }
}

fn ordered(a: f64, b: f64) -> (f64, f64) {
    (a.min(b), a.max(b))
}

/// Half-open cell index range covering a world span, clamped to the grid.
fn cell_range(lo: f64, hi: f64, origin: f64, voxel_size: f32, count: usize) -> (usize, usize) {
    let v = voxel_size as f64;
    let start = ((lo - origin) / v).floor().max(0.0) as usize;
    let end = (((hi - origin) / v).ceil().max(0.0) as usize + 1).min(count);
    (start.min(count), end)
}

/// Convenience for callers that only have a `Vec3`.
pub fn plan_point(point: Vec3) -> Position {
    Position::new(point.x as f32, point.z as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::cad::{CAD_DOOR_HEIGHT, CAD_DOOR_WIDTH};
    use crate::domain::entities::voxel_grid::VOXEL_WALL;

    const VOXEL: f32 = 0.2;

    fn grid() -> VoxelGrid {
        // 8 x 3 x 8 world units at 0.2.
        VoxelGrid::new(40, 15, 40)
    }

    #[test]
    fn a_wall_is_centered_on_the_line_the_author_drew() {
        let mut session = AuthoringSession::new();
        session.wall(
            Position::new(1.0, 4.0),
            Position::new(7.0, 4.0),
            0.4,
            2.6,
            VOXEL_WALL,
        );
        let mut g = grid();
        session.apply_to_grid(&mut g, Position::new(0.0, 0.0), VOXEL);

        // Solid on the centerline, air a clear metre either side of it.
        let cell = |wx: f32, wy: f32, wz: f32| {
            g.get(
                (wx / VOXEL) as usize,
                (wy / VOXEL) as usize,
                (wz / VOXEL) as usize,
            )
        };
        assert_eq!(cell(4.0, 1.0, 4.0), VOXEL_WALL);
        assert_eq!(cell(4.0, 1.0, 3.0), VOXEL_AIR);
        assert_eq!(cell(4.0, 1.0, 5.0), VOXEL_AIR);
    }

    #[test]
    fn an_authored_doorway_is_actually_passable() {
        // The rule this module exists for: a cut at exactly the finished
        // width can quantize to less than a door, at some grid phases. Walk
        // the door across sub-voxel offsets so a lucky alignment cannot pass
        // this by accident.
        for step in 0..5 {
            let phase = step as f32 * (VOXEL / 5.0);
            let mut session = AuthoringSession::new();
            session
                .wall(
                    Position::new(0.0, 4.0),
                    Position::new(8.0, 4.0),
                    0.4,
                    2.6,
                    VOXEL_WALL,
                )
                .doorway(
                    Position::new(4.0 + phase, 4.0),
                    CAD_DOOR_WIDTH,
                    CAD_DOOR_HEIGHT,
                    1.0,
                    VOXEL,
                    true,
                );
            let mut g = grid();
            session.apply_to_grid(&mut g, Position::new(0.0, 0.0), VOXEL);

            // Count the clear width at head height along the wall line.
            let zi = (4.0 / VOXEL) as usize;
            let yi = (1.0 / VOXEL) as usize;
            let clear = (0..g.width())
                .filter(|&xi| g.get(xi, yi, zi) == VOXEL_AIR)
                .count();
            let clear_units = clear as f32 * VOXEL;
            assert!(
                clear_units >= CAD_DOOR_WIDTH,
                "phase {phase}: doorway quantized to {clear_units} u, under the \
                 {CAD_DOOR_WIDTH} u finished width"
            );
        }
    }

    #[test]
    fn a_doorway_does_not_cut_the_whole_wall_down() {
        let mut session = AuthoringSession::new();
        session
            .wall(
                Position::new(0.0, 4.0),
                Position::new(8.0, 4.0),
                0.4,
                2.6,
                VOXEL_WALL,
            )
            .doorway(
                Position::new(4.0, 4.0),
                CAD_DOOR_WIDTH,
                CAD_DOOR_HEIGHT,
                1.0,
                VOXEL,
                true,
            );
        let mut g = grid();
        session.apply_to_grid(&mut g, Position::new(0.0, 0.0), VOXEL);
        let zi = (4.0 / VOXEL) as usize;
        let yi = (1.0 / VOXEL) as usize;
        // Wall survives well away from the opening...
        assert_eq!(g.get((0.6 / VOXEL) as usize, yi, zi), VOXEL_WALL);
        assert_eq!(g.get((7.4 / VOXEL) as usize, yi, zi), VOXEL_WALL);
        // ...and there is a header above the door.
        let head = ((CAD_DOOR_HEIGHT + 2.0 * VOXEL) / VOXEL) as usize;
        assert_eq!(g.get((4.0 / VOXEL) as usize, head, zi), VOXEL_WALL);
    }

    #[test]
    fn order_is_the_edit_history() {
        // Cutting before building leaves the wall whole: the cut had nothing
        // to remove. This is what makes the session a history rather than a
        // set, and it is why undo can just pop.
        let mut session = AuthoringSession::new();
        session
            .doorway(
                Position::new(4.0, 4.0),
                CAD_DOOR_WIDTH,
                CAD_DOOR_HEIGHT,
                1.0,
                VOXEL,
                true,
            )
            .wall(
                Position::new(0.0, 4.0),
                Position::new(8.0, 4.0),
                0.4,
                2.6,
                VOXEL_WALL,
            );
        let mut g = grid();
        session.apply_to_grid(&mut g, Position::new(0.0, 0.0), VOXEL);
        let zi = (4.0 / VOXEL) as usize;
        let yi = (1.0 / VOXEL) as usize;
        assert_eq!(
            g.get((4.0 / VOXEL) as usize, yi, zi),
            VOXEL_WALL,
            "a cut made before the wall existed removed something"
        );
    }

    #[test]
    fn undo_removes_the_last_operation_only() {
        let mut session = AuthoringSession::new();
        session
            .wall(
                Position::new(0.0, 4.0),
                Position::new(8.0, 4.0),
                0.4,
                2.6,
                VOXEL_WALL,
            )
            .doorway(
                Position::new(4.0, 4.0),
                CAD_DOOR_WIDTH,
                CAD_DOOR_HEIGHT,
                1.0,
                VOXEL,
                true,
            );
        assert_eq!(session.elements().len(), 2);
        session.undo();
        assert_eq!(session.elements().len(), 1);

        let mut g = grid();
        session.apply_to_grid(&mut g, Position::new(0.0, 0.0), VOXEL);
        let zi = (4.0 / VOXEL) as usize;
        let yi = (1.0 / VOXEL) as usize;
        assert_eq!(
            g.get((4.0 / VOXEL) as usize, yi, zi),
            VOXEL_WALL,
            "undoing the doorway did not restore the wall"
        );
    }

    #[test]
    fn authored_geometry_lands_in_world_space_across_chunk_seams() {
        // The same wall written into two adjacent chunks must be continuous:
        // an author's wall does not stop because a streaming boundary does.
        let mut session = AuthoringSession::new();
        session.wall(
            Position::new(2.0, 4.0),
            Position::new(14.0, 4.0),
            0.4,
            2.6,
            VOXEL_WALL,
        );
        let mut left = grid();
        let mut right = grid();
        session.apply_to_grid(&mut left, Position::new(0.0, 0.0), VOXEL);
        session.apply_to_grid(&mut right, Position::new(8.0, 0.0), VOXEL);

        let zi = (4.0 / VOXEL) as usize;
        let yi = (1.0 / VOXEL) as usize;
        // Last column of the left chunk and first of the right are both wall.
        assert_eq!(left.get(left.width() - 1, yi, zi), VOXEL_WALL);
        assert_eq!(right.get(0, yi, zi), VOXEL_WALL);
    }

    #[test]
    fn rough_openings_are_never_smaller_than_what_was_asked_for() {
        for finished in [0.815f32, 0.9, 1.2, 1.8] {
            for voxel in [0.1f32, 0.2, 0.4] {
                assert!(rough_opening(finished, voxel) > finished);
            }
        }
    }
}
