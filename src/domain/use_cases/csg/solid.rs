//! Author-facing solids: the SketchUp-verbs surface of the CSG kernel.
//! Build primitives (or extrude a sketch), combine them with booleans,
//! query containment, and hand the result to a voxelizer.

use super::bsp::BspNode;
use super::polygon::Polygon;
use super::vec3::{Vec3, v3};

/// A closed solid as a boundary polygon soup. Constructors and boolean
/// operations preserve closedness; open shells are undefined behavior for
/// containment queries.
#[derive(Debug, Clone, Default)]
pub struct Solid {
    pub polygons: Vec<Polygon>,
}

impl Solid {
    /// Axis-aligned box between two corners.
    pub fn cuboid(min: Vec3, max: Vec3) -> Solid {
        let corner = |mask: u8| {
            v3(
                if mask & 1 != 0 { max.x } else { min.x },
                if mask & 2 != 0 { max.y } else { min.y },
                if mask & 4 != 0 { max.z } else { min.z },
            )
        };
        // Each face wound counter-clockwise seen from outside.
        let faces: [[u8; 4]; 6] = [
            [1, 3, 7, 5], // +X
            [0, 4, 6, 2], // -X
            [2, 6, 7, 3], // +Y
            [0, 1, 5, 4], // -Y
            [4, 5, 7, 6], // +Z
            [0, 2, 3, 1], // -Z
        ];
        Solid {
            polygons: faces
                .iter()
                .map(|face| Polygon::new(face.iter().map(|&i| corner(i)).collect()))
                .collect(),
        }
    }

    /// Push/pull: sweeps a convex planar profile along `direction` by
    /// `distance`, producing the closed prism (SketchUp's defining verb).
    /// Either winding of the profile is accepted; the sweep must not be
    /// parallel to the profile plane.
    pub fn extrude(profile: &[Vec3], direction: Vec3, distance: f64) -> Solid {
        assert!(profile.len() >= 3, "a sketch profile needs three points");
        let sweep = direction.normalized() * distance;

        // Normalize orientation: make the profile face against the sweep,
        // so it is the outward-facing bottom cap verbatim.
        let mut base = profile.to_vec();
        let facing = Polygon::new(base.clone()).plane.normal.dot(sweep);
        assert!(
            facing.abs() > 1.0e-9,
            "sweep direction lies in the profile plane"
        );
        if facing > 0.0 {
            base.reverse();
        }

        let count = base.len();
        let top: Vec<Vec3> = base.iter().map(|&p| p + sweep).collect();
        let mut polygons = Vec::with_capacity(count + 2);
        polygons.push(Polygon::new(base.clone()));
        let mut top_cap = top.clone();
        top_cap.reverse();
        polygons.push(Polygon::new(top_cap));
        for i in 0..count {
            let j = (i + 1) % count;
            polygons.push(Polygon::new(vec![base[i], top[i], top[j], base[j]]));
        }
        Solid { polygons }
    }

    /// The BSP index of this solid, for repeated containment queries.
    pub fn bsp(&self) -> BspNode {
        BspNode::from_polygons(self.polygons.clone())
    }

    /// One-off containment check; build [`Self::bsp`] once when sampling
    /// many points.
    pub fn contains_point(&self, point: Vec3) -> bool {
        self.bsp().contains_point(point)
    }

    /// Axis-aligned bounds, `None` for the empty solid.
    pub fn aabb(&self) -> Option<(Vec3, Vec3)> {
        let mut vertices = self.polygons.iter().flat_map(|p| p.vertices.iter());
        let first = *vertices.next()?;
        let (mut lo, mut hi) = (first, first);
        for &vertex in vertices {
            lo = v3(lo.x.min(vertex.x), lo.y.min(vertex.y), lo.z.min(vertex.z));
            hi = v3(hi.x.max(vertex.x), hi.y.max(vertex.y), hi.z.max(vertex.z));
        }
        Some((lo, hi))
    }

    /// Everything in either solid.
    pub fn union(&self, other: &Solid) -> Solid {
        let (mut a, mut b) = (self.bsp(), other.bsp());
        a.clip_to(&b);
        b.clip_to(&a);
        b.invert();
        b.clip_to(&a);
        b.invert();
        let mut polygons = a.all_polygons();
        polygons.extend(b.all_polygons());
        Solid { polygons }
    }

    /// Everything in `self` not in `other` — the doorway-cutting verb.
    pub fn subtract(&self, other: &Solid) -> Solid {
        let (mut a, mut b) = (self.bsp(), other.bsp());
        a.invert();
        a.clip_to(&b);
        b.clip_to(&a);
        b.invert();
        b.clip_to(&a);
        b.invert();
        let mut polygons = a.all_polygons();
        polygons.extend(b.all_polygons());
        let mut result = BspNode::from_polygons(polygons);
        result.invert();
        Solid {
            polygons: result.all_polygons(),
        }
    }

    /// Only the overlap of both solids.
    pub fn intersect(&self, other: &Solid) -> Solid {
        let (mut a, mut b) = (self.bsp(), other.bsp());
        a.invert();
        b.clip_to(&a);
        b.invert();
        a.clip_to(&b);
        b.clip_to(&a);
        let mut polygons = a.all_polygons();
        polygons.extend(b.all_polygons());
        let mut result = BspNode::from_polygons(polygons);
        result.invert();
        Solid {
            polygons: result.all_polygons(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::cad::{CAD_DOOR_HEIGHT, CAD_DOOR_WIDTH};

    #[test]
    fn cuboid_contains_its_interior_and_boundary_but_not_outside() {
        let cube = Solid::cuboid(v3(0.0, 0.0, 0.0), v3(2.0, 2.0, 2.0));
        assert!(cube.contains_point(v3(1.0, 1.0, 1.0)));
        assert!(cube.contains_point(v3(2.0, 1.0, 1.0)), "surface is solid");
        assert!(!cube.contains_point(v3(2.5, 1.0, 1.0)));
        assert!(!cube.contains_point(v3(-0.1, 1.0, 1.0)));
    }

    #[test]
    fn doorway_subtraction_opens_a_walkable_hole_and_keeps_the_lintel() {
        // A 4 m wall slab; a CAD-dimensioned door volume cut through it.
        let wall = Solid::cuboid(v3(0.0, 0.0, 0.0), v3(4.0, 3.0, 0.2));
        let half_width = CAD_DOOR_WIDTH as f64 / 2.0;
        let door = Solid::cuboid(
            v3(2.0 - half_width, 0.0, -0.1),
            v3(2.0 + half_width, CAD_DOOR_HEIGHT as f64, 0.3),
        );
        let doored = wall.subtract(&door);
        let bsp = doored.bsp();

        assert!(!bsp.contains_point(v3(2.0, 1.0, 0.1)), "opening is clear");
        assert!(!bsp.contains_point(v3(2.0, 2.0, 0.1)), "head height clear");
        assert!(bsp.contains_point(v3(2.0, 2.5, 0.1)), "lintel remains");
        assert!(bsp.contains_point(v3(0.5, 1.0, 0.1)), "jamb wall remains");
        assert!(bsp.contains_point(v3(3.5, 1.0, 0.1)), "far jamb remains");
    }

    #[test]
    fn union_covers_both_and_intersection_only_the_overlap() {
        let a = Solid::cuboid(v3(0.0, 0.0, 0.0), v3(2.0, 2.0, 2.0));
        let b = Solid::cuboid(v3(1.0, 0.0, 0.0), v3(3.0, 2.0, 2.0));

        let merged = a.union(&b).bsp();
        assert!(merged.contains_point(v3(0.5, 1.0, 1.0)));
        assert!(merged.contains_point(v3(2.5, 1.0, 1.0)));
        assert!(!merged.contains_point(v3(3.5, 1.0, 1.0)));

        let overlap = a.intersect(&b).bsp();
        assert!(overlap.contains_point(v3(1.5, 1.0, 1.0)));
        assert!(!overlap.contains_point(v3(0.5, 1.0, 1.0)));
        assert!(!overlap.contains_point(v3(2.5, 1.0, 1.0)));
    }

    #[test]
    fn push_pull_extrusion_builds_the_closed_prism() {
        // An L-free convex sketch: a right triangle swept upward.
        let profile = [v3(0.0, 0.0, 0.0), v3(2.0, 0.0, 0.0), v3(0.0, 0.0, 2.0)];
        let prism = Solid::extrude(&profile, v3(0.0, 1.0, 0.0), 3.0);
        assert_eq!(prism.polygons.len(), 5, "two caps and three sides");
        let bsp = prism.bsp();
        assert!(bsp.contains_point(v3(0.5, 1.5, 0.5)));
        assert!(!bsp.contains_point(v3(0.5, 3.5, 0.5)), "above the sweep");
        assert!(!bsp.contains_point(v3(1.5, 1.5, 1.5)), "off the hypotenuse");
        let (lo, hi) = prism.aabb().unwrap();
        assert_eq!((lo.y, hi.y), (0.0, 3.0));
    }
}
