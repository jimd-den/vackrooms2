//! Convex planar polygons: the boundary representation the BSP splits.

use super::plane::Plane;
use super::vec3::Vec3;

/// A convex polygon with its containing plane. Vertices wind counter-
/// clockwise when viewed from the plane's front side. The splitting
/// algorithm requires convexity; authoring APIs must triangulate or fan
/// concave profiles before reaching here.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon {
    pub vertices: Vec<Vec3>,
    pub plane: Plane,
}

impl Polygon {
    /// A polygon from at least three non-collinear vertices; the plane is
    /// derived from the first three.
    pub fn new(vertices: Vec<Vec3>) -> Polygon {
        assert!(vertices.len() >= 3, "a polygon needs three vertices");
        let plane = Plane::from_points(vertices[0], vertices[1], vertices[2]);
        Polygon { vertices, plane }
    }

    /// Keeps a known plane instead of re-deriving it — split fragments must
    /// inherit their parent's exact plane or round-off drifts per cut.
    pub fn from_vertices_with_plane(vertices: Vec<Vec3>, plane: Plane) -> Polygon {
        Polygon { vertices, plane }
    }

    /// The same boundary facing the other way.
    pub fn flipped(&self) -> Polygon {
        let mut vertices = self.vertices.clone();
        vertices.reverse();
        Polygon {
            vertices,
            plane: self.plane.flipped(),
        }
    }
}
