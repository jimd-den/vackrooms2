//! Solid-leaf BSP tree: the spatial index behind boolean solid operations.
//!
//! Convention (inherited from the classic csg.js formulation): a region
//! with no `front` child is outside the solid, a region with no `back`
//! child is inside it. `clip_polygons` discards fragments that land in
//! solid regions, which is the single primitive union/subtract/intersect
//! are built from.

use super::plane::{PLANE_EPSILON, Plane};
use super::polygon::Polygon;

#[derive(Debug, Clone, Default)]
pub struct BspNode {
    plane: Option<Plane>,
    front: Option<Box<BspNode>>,
    back: Option<Box<BspNode>>,
    polygons: Vec<Polygon>,
}

impl BspNode {
    pub fn from_polygons(polygons: Vec<Polygon>) -> BspNode {
        let mut node = BspNode::default();
        node.build(polygons);
        node
    }

    /// Inserts polygons, extending the tree wherever they cross regions.
    pub fn build(&mut self, polygons: Vec<Polygon>) {
        if polygons.is_empty() {
            return;
        }
        let plane = *self.plane.get_or_insert_with(|| polygons[0].plane);

        let mut front = Vec::new();
        let mut back = Vec::new();
        // Coplanar fragments live on this node, facing either way.
        let mut coplanar_front = Vec::new();
        let mut coplanar_back = Vec::new();
        for polygon in &polygons {
            plane.split_polygon(
                polygon,
                &mut coplanar_front,
                &mut coplanar_back,
                &mut front,
                &mut back,
            );
        }
        self.polygons.extend(coplanar_front);
        self.polygons.extend(coplanar_back);
        if !front.is_empty() {
            self.front.get_or_insert_with(Box::default).build(front);
        }
        if !back.is_empty() {
            self.back.get_or_insert_with(Box::default).build(back);
        }
    }

    /// Converts solid space to empty space and back.
    pub fn invert(&mut self) {
        for polygon in &mut self.polygons {
            *polygon = polygon.flipped();
        }
        if let Some(plane) = &mut self.plane {
            *plane = plane.flipped();
        }
        if let Some(front) = &mut self.front {
            front.invert();
        }
        if let Some(back) = &mut self.back {
            back.invert();
        }
        std::mem::swap(&mut self.front, &mut self.back);
    }

    /// Returns the fragments of `polygons` lying in empty space of this
    /// tree; fragments inside the solid are removed.
    pub fn clip_polygons(&self, polygons: Vec<Polygon>) -> Vec<Polygon> {
        let Some(plane) = self.plane else {
            return polygons;
        };
        let mut front = Vec::new();
        let mut back = Vec::new();
        let mut coplanar_front = Vec::new();
        let mut coplanar_back = Vec::new();
        for polygon in &polygons {
            plane.split_polygon(
                polygon,
                &mut coplanar_front,
                &mut coplanar_back,
                &mut front,
                &mut back,
            );
        }
        // Coplanar fragments clip with the region they face into.
        front.extend(coplanar_front);
        back.extend(coplanar_back);
        let mut result = match &self.front {
            Some(node) => node.clip_polygons(front),
            None => front, // no front child: region is outside, keep
        };
        if let Some(node) = &self.back {
            result.extend(node.clip_polygons(back));
        } // no back child: region is inside the solid, discard
        result
    }

    /// Removes every part of this tree's polygons inside `other`'s solid.
    pub fn clip_to(&mut self, other: &BspNode) {
        self.polygons = other.clip_polygons(std::mem::take(&mut self.polygons));
        if let Some(front) = &mut self.front {
            front.clip_to(other);
        }
        if let Some(back) = &mut self.back {
            back.clip_to(other);
        }
    }

    /// Every polygon in the tree.
    pub fn all_polygons(&self) -> Vec<Polygon> {
        let mut polygons = self.polygons.clone();
        if let Some(front) = &self.front {
            polygons.extend(front.all_polygons());
        }
        if let Some(back) = &self.back {
            polygons.extend(back.all_polygons());
        }
        polygons
    }

    /// Is `point` inside the solid this tree bounds? Points within epsilon
    /// of a boundary count as inside, so voxelization fills surfaces.
    pub fn contains_point(&self, point: super::vec3::Vec3) -> bool {
        let Some(plane) = self.plane else {
            // An empty tree bounds no solid.
            return false;
        };
        let distance = plane.distance(point);
        if distance > PLANE_EPSILON {
            match &self.front {
                Some(node) => node.contains_point(point),
                None => false,
            }
        } else if distance < -PLANE_EPSILON {
            match &self.back {
                Some(node) => node.contains_point(point),
                None => true,
            }
        } else {
            // On the cut plane: solid if either adjacent region is solid.
            let front_solid = self
                .front
                .as_ref()
                .is_some_and(|node| node.contains_point(point));
            let back_solid = self
                .back
                .as_ref()
                .is_none_or(|node| node.contains_point(point));
            front_solid || back_solid
        }
    }
}
