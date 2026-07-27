//! Oriented partitioning planes: the half-space primitive every CSG
//! operation reduces to.

use super::polygon::Polygon;
use super::vec3::Vec3;

/// Distance under which a point counts as lying on the plane.
pub const PLANE_EPSILON: f64 = 1.0e-5;

const COPLANAR: u8 = 0;
const FRONT: u8 = 1;
const BACK: u8 = 2;
const SPANNING: u8 = 3;

/// `normal . p == w` for points on the plane; `> w` is the front half-space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plane {
    pub normal: Vec3,
    pub w: f64,
}

impl Plane {
    /// The plane through three points, front side following the right-hand
    /// winding of (a, b, c).
    pub fn from_points(a: Vec3, b: Vec3, c: Vec3) -> Plane {
        let normal = (b - a).cross(c - a).normalized();
        Plane {
            normal,
            w: normal.dot(a),
        }
    }

    pub fn flipped(self) -> Plane {
        Plane {
            normal: -self.normal,
            w: -self.w,
        }
    }

    /// Signed distance of `point` from the plane (positive in front).
    pub fn distance(self, point: Vec3) -> f64 {
        self.normal.dot(point) - self.w
    }

    /// Splits `polygon` by this plane into the four possible destinations.
    /// Coplanar polygons go front or back by facing; spanning polygons are
    /// cut along the intersection with new vertices on the plane.
    pub fn split_polygon(
        self,
        polygon: &Polygon,
        coplanar_front: &mut Vec<Polygon>,
        coplanar_back: &mut Vec<Polygon>,
        front: &mut Vec<Polygon>,
        back: &mut Vec<Polygon>,
    ) {
        let mut polygon_type = COPLANAR;
        let types: Vec<u8> = polygon
            .vertices
            .iter()
            .map(|&vertex| {
                let t = self.distance(vertex);
                let vertex_type = if t < -PLANE_EPSILON {
                    BACK
                } else if t > PLANE_EPSILON {
                    FRONT
                } else {
                    COPLANAR
                };
                polygon_type |= vertex_type;
                vertex_type
            })
            .collect();

        match polygon_type {
            COPLANAR => {
                if self.normal.dot(polygon.plane.normal) > 0.0 {
                    coplanar_front.push(polygon.clone());
                } else {
                    coplanar_back.push(polygon.clone());
                }
            }
            FRONT => front.push(polygon.clone()),
            BACK => back.push(polygon.clone()),
            _ => {
                let mut front_vertices = Vec::new();
                let mut back_vertices = Vec::new();
                let count = polygon.vertices.len();
                for i in 0..count {
                    let j = (i + 1) % count;
                    let (ti, tj) = (types[i], types[j]);
                    let (vi, vj) = (polygon.vertices[i], polygon.vertices[j]);
                    if ti != BACK {
                        front_vertices.push(vi);
                    }
                    if ti != FRONT {
                        back_vertices.push(vi);
                    }
                    if (ti | tj) == SPANNING {
                        let t = -self.distance(vi) / self.normal.dot(vj - vi);
                        let crossing = vi.lerp(vj, t);
                        front_vertices.push(crossing);
                        back_vertices.push(crossing);
                    }
                }
                if front_vertices.len() >= 3 {
                    front.push(Polygon::from_vertices_with_plane(
                        front_vertices,
                        polygon.plane,
                    ));
                }
                if back_vertices.len() >= 3 {
                    back.push(Polygon::from_vertices_with_plane(back_vertices, polygon.plane));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::vec3::v3;
    use super::*;

    fn unit_quad_at_x(x: f64) -> Polygon {
        Polygon::new(vec![
            v3(x, 0.0, 0.0),
            v3(x, 1.0, 0.0),
            v3(x, 1.0, 1.0),
            v3(x, 0.0, 1.0),
        ])
    }

    #[test]
    fn whole_side_polygons_route_without_cutting() {
        let plane = Plane::from_points(v3(0.0, 0.0, 0.0), v3(0.0, 0.0, 1.0), v3(0.0, 1.0, 0.0));
        let (mut cf, mut cb, mut f, mut b) = (vec![], vec![], vec![], vec![]);
        plane.split_polygon(&unit_quad_at_x(1.0), &mut cf, &mut cb, &mut f, &mut b);
        plane.split_polygon(&unit_quad_at_x(-1.0), &mut cf, &mut cb, &mut f, &mut b);
        assert_eq!((f.len(), b.len(), cf.len(), cb.len()), (1, 1, 0, 0));
    }

    #[test]
    fn spanning_polygons_split_with_vertices_on_the_plane() {
        // A quad crossing x = 0 splits into two halves that share an edge
        // exactly on the plane.
        let plane = Plane::from_points(v3(0.0, 0.0, 0.0), v3(0.0, 0.0, 1.0), v3(0.0, 1.0, 0.0));
        let quad = Polygon::new(vec![
            v3(-1.0, 0.0, 0.0),
            v3(1.0, 0.0, 0.0),
            v3(1.0, 0.0, 1.0),
            v3(-1.0, 0.0, 1.0),
        ]);
        let (mut cf, mut cb, mut f, mut b) = (vec![], vec![], vec![], vec![]);
        plane.split_polygon(&quad, &mut cf, &mut cb, &mut f, &mut b);
        assert_eq!((f.len(), b.len()), (1, 1));
        for half in f.iter().chain(b.iter()) {
            let on_plane = half
                .vertices
                .iter()
                .filter(|&&vertex| plane.distance(vertex).abs() <= PLANE_EPSILON)
                .count();
            assert_eq!(on_plane, 2, "each half keeps the shared cut edge");
        }
    }

    #[test]
    fn coplanar_polygons_route_by_facing() {
        let plane = Plane::from_points(v3(0.0, 0.0, 0.0), v3(0.0, 0.0, 1.0), v3(0.0, 1.0, 0.0));
        let facing_same = unit_quad_at_x(0.0);
        let facing_opposite =
            Polygon::from_vertices_with_plane(facing_same.vertices.clone(), facing_same.plane)
                .flipped();
        let (mut cf, mut cb, mut f, mut b) = (vec![], vec![], vec![], vec![]);
        plane.split_polygon(&facing_same, &mut cf, &mut cb, &mut f, &mut b);
        plane.split_polygon(&facing_opposite, &mut cf, &mut cb, &mut f, &mut b);
        assert_eq!((cf.len(), cb.len()), (1, 1));
    }
}
