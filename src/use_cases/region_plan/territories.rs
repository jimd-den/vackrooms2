//! Free-space decomposition: what is left of a region once circulation and
//! already-committed masses are removed.
//!
//! Room placement needs somewhere to place rooms. The old cursor walk did
//! without this by sliding along a corridor leg and trying one spot at a
//! time, which is why a region ended up with two suites and 90% unplanned
//! fabric: a spot that failed was simply skipped, and space away from the
//! dominant leg was never considered at all.
//!
//! This produces the candidate set the placement solver scores over:
//! maximal free rectangles, biggest first, each carved out of the free area
//! before the next is found. Rectangles rather than arbitrary polygons
//! because rooms in this building are rectangular, and a candidate that
//! cannot host a room is not a useful candidate.

use crate::domain::entities::architecture::CirculationSpine;

/// Free-space lattice pitch. Two coarse voxels: fine enough that a wall
/// band cannot hide between samples, coarse enough that a region is a
/// 100x100 grid rather than a 200x200 one.
const CELL: f32 = 0.8;

/// Smallest side a territory may have and still be offered as a candidate.
/// Below a corridor's width there is nothing a room could occupy.
const MIN_SIDE: f32 = 3.2;

/// An axis-aligned patch of unoccupied region floor, in world units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Territory {
    pub min_x: f32,
    pub min_z: f32,
    pub max_x: f32,
    pub max_z: f32,
}

impl Territory {
    pub fn width(&self) -> f32 {
        self.max_x - self.min_x
    }

    pub fn depth(&self) -> f32 {
        self.max_z - self.min_z
    }

    pub fn area(&self) -> f32 {
        self.width() * self.depth()
    }

    pub fn center(&self) -> (f32, f32) {
        (
            (self.min_x + self.max_x) * 0.5,
            (self.min_z + self.max_z) * 0.5,
        )
    }

    /// Only the overlap assertions need a territory as a raw tuple; the
    /// solver scores territories through `center`/`width`/`depth`.
    #[cfg(test)]
    pub fn bounds(&self) -> (f32, f32, f32, f32) {
        (self.min_x, self.min_z, self.max_x, self.max_z)
    }

    fn overlaps(&self, other: &(f32, f32, f32, f32)) -> bool {
        self.min_x < other.2 && other.0 < self.max_x && self.min_z < other.3 && other.1 < self.max_z
    }
}

/// Decomposes the region's unoccupied floor into candidate territories,
/// largest first.
///
/// Pure in its inputs, so two chunks asking about the same region get the
/// same answer without coordinating -- the same discipline the rest of the
/// planner keeps.
pub fn free_territories(
    origin_x: f32,
    origin_z: f32,
    region_size: f32,
    corridors: &[CirculationSpine],
    taken: &[(f32, f32, f32, f32)],
    edge_margin: f32,
    wall_thickness: f32,
) -> Vec<Territory> {
    let usable = region_size - 2.0 * edge_margin;
    if usable <= MIN_SIDE {
        return Vec::new();
    }
    let n = (usable / CELL).floor() as usize;
    if n == 0 {
        return Vec::new();
    }
    let base_x = origin_x + edge_margin;
    let base_z = origin_z + edge_margin;

    // A cell is free when its centre clears every corridor by half a width
    // plus a wall, and falls in no committed footprint. Corridors are kept
    // clear rather than merely un-overlapped: a room flush against a
    // corridor edge has no wall between it and the route.
    let mut free = vec![true; n * n];
    for iz in 0..n {
        for ix in 0..n {
            let x = base_x + (ix as f32 + 0.5) * CELL;
            let z = base_z + (iz as f32 + 0.5) * CELL;
            let blocked = corridors
                .iter()
                .any(|s| s.distance(x, z) < s.width * 0.5 + wall_thickness)
                || taken
                    .iter()
                    .any(|b| x >= b.0 && x <= b.2 && z >= b.1 && z <= b.3);
            free[iz * n + ix] = !blocked;
        }
    }

    let min_cells = (MIN_SIDE / CELL).ceil() as usize;
    let mut out = Vec::new();
    // Repeatedly take the largest all-free rectangle and remove it. Greedy
    // maximal-rectangle carving, not an exact partition: the aim is a good
    // candidate set for scoring, and the scorer is free to use only part of
    // whatever it is offered.
    while let Some(rect) = largest_free_rectangle(&free, n, min_cells) {
        let (ix0, iz0, ix1, iz1) = rect;
        for iz in iz0..=iz1 {
            for ix in ix0..=ix1 {
                free[iz * n + ix] = false;
            }
        }
        let territory = Territory {
            min_x: base_x + ix0 as f32 * CELL,
            min_z: base_z + iz0 as f32 * CELL,
            max_x: base_x + (ix1 + 1) as f32 * CELL,
            max_z: base_z + (iz1 + 1) as f32 * CELL,
        };
        if taken.iter().any(|b| territory.overlaps(b)) {
            continue;
        }
        out.push(territory);
        if out.len() >= 64 {
            break;
        }
    }
    out
}

/// Largest all-free axis-aligned rectangle in the mask, as inclusive cell
/// indices. Classic largest-rectangle-in-histogram swept down the rows:
/// O(n^2) for the whole grid rather than the O(n^4) of testing every
/// rectangle.
fn largest_free_rectangle(
    free: &[bool],
    n: usize,
    min_cells: usize,
) -> Option<(usize, usize, usize, usize)> {
    let mut heights = vec![0usize; n];
    let mut best: Option<(usize, usize, usize, usize, usize)> = None; // + area

    for iz in 0..n {
        for ix in 0..n {
            heights[ix] = if free[iz * n + ix] {
                heights[ix] + 1
            } else {
                0
            };
        }
        // Monotonic stack over this row's histogram.
        let mut stack: Vec<(usize, usize)> = Vec::new(); // (start index, height)
        for ix in 0..=n {
            let height = if ix == n { 0 } else { heights[ix] };
            let mut start = ix;
            while let Some(&(top_start, top_height)) = stack.last() {
                if top_height <= height {
                    break;
                }
                stack.pop();
                let width = ix - top_start;
                let area = width * top_height;
                if width >= min_cells
                    && top_height >= min_cells
                    && best.is_none_or(|(_, _, _, _, b)| area > b)
                {
                    best = Some((top_start, iz + 1 - top_height, ix - 1, iz, area));
                }
                start = top_start;
            }
            if ix < n {
                stack.push((start, height));
            }
        }
    }

    best.map(|(x0, z0, x1, z1, _)| (x0, z0, x1, z1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::architecture::SpaceProgram;
    use crate::domain::entities::position::Position;

    fn spine(x0: f32, z0: f32, x1: f32, z1: f32, width: f32) -> CirculationSpine {
        CirculationSpine {
            id: 0,
            path: vec![Position::new(x0, z0), Position::new(x1, z1)],
            width,
            spine_kind: SpaceProgram::MainCorridor,
        }
    }

    #[test]
    fn an_empty_region_yields_one_big_territory() {
        let out = free_territories(0.0, 0.0, 80.0, &[], &[], 3.2, 0.5);
        assert!(!out.is_empty());
        let first = out[0];
        // The whole usable square, less lattice rounding.
        assert!(
            first.area() > 70.0 * 70.0,
            "expected near-full coverage, got {} x {}",
            first.width(),
            first.depth()
        );
    }

    #[test]
    fn a_corridor_splits_the_region_and_is_never_covered() {
        let corridors = [spine(0.0, 40.0, 80.0, 40.0, 4.0)];
        let out = free_territories(0.0, 0.0, 80.0, &corridors, &[], 3.2, 0.5);
        assert!(out.len() >= 2, "a crossing corridor must split the floor");
        for territory in &out {
            let (cx, cz) = territory.center();
            assert!(
                corridors[0].distance(cx, cz) >= 2.0,
                "territory at ({cx}, {cz}) sits on the corridor"
            );
            // No territory may straddle the route either.
            assert!(
                territory.max_z <= 40.0 - 2.0 || territory.min_z >= 40.0 + 2.0,
                "territory {territory:?} crosses the corridor"
            );
        }
    }

    #[test]
    fn committed_footprints_are_excluded() {
        let taken = [(10.0, 10.0, 30.0, 30.0)];
        let out = free_territories(0.0, 0.0, 80.0, &[], &taken, 3.2, 0.5);
        assert!(!out.is_empty());
        for territory in &out {
            assert!(
                !territory.overlaps(&taken[0]),
                "territory {territory:?} overlaps a committed mass"
            );
        }
    }

    #[test]
    fn territories_never_overlap_each_other() {
        let corridors = [
            spine(0.0, 40.0, 80.0, 40.0, 4.0),
            spine(40.0, 0.0, 40.0, 80.0, 4.0),
        ];
        let out = free_territories(0.0, 0.0, 80.0, &corridors, &[], 3.2, 0.5);
        assert!(out.len() >= 4, "a cross must yield four quadrants");
        for (i, a) in out.iter().enumerate() {
            for b in &out[i + 1..] {
                assert!(
                    !a.overlaps(&b.bounds()),
                    "territories {a:?} and {b:?} overlap"
                );
            }
        }
    }

    #[test]
    fn slivers_below_the_minimum_side_are_not_offered() {
        let out = free_territories(0.0, 0.0, 80.0, &[], &[], 3.2, 0.5);
        for territory in &out {
            assert!(
                territory.width() >= MIN_SIDE && territory.depth() >= MIN_SIDE,
                "sliver offered as a candidate: {territory:?}"
            );
        }
    }

    #[test]
    fn decomposition_is_deterministic() {
        let corridors = [spine(0.0, 30.0, 80.0, 30.0, 4.0)];
        let a = free_territories(0.0, 0.0, 80.0, &corridors, &[], 3.2, 0.5);
        let b = free_territories(0.0, 0.0, 80.0, &corridors, &[], 3.2, 0.5);
        assert_eq!(a, b);
    }
}
