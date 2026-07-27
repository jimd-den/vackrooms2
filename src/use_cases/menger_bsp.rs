//! Menger-style recursive space partitioning for macro structure.
//!
//! A floor plate splits into a 3x3 lattice of cells; the center cell is
//! carved out as primary circulation (the Menger "cutout"), and each ring
//! cell either recurses — growing finer corridor loops — or stays a leaf
//! room. Depth-limited and span-limited by human passage dimensions, so the
//! recursion can never produce a hall too narrow to walk.
//!
//! Pure and deterministic: structure is a function of (bounds, seed) alone,
//! so overlapping windows regenerate identical partitions — the same
//! seam-freedom contract the level generators rely on.

use crate::domain::entities::anomaly::WorldBounds;
use crate::domain::entities::cad::CAD_CORRIDOR_WIDTH;
use crate::use_cases::anomalies::determinism::{hash, unit};

/// One node of the partition tree. Leaves are rooms or corridor segments;
/// internal nodes carry their nine children in row-major (x, z) order.
#[derive(Debug, Clone)]
pub struct MengerNode {
    pub bounds: WorldBounds,
    pub depth: u32,
    pub is_corridor: bool,
    pub children: Vec<MengerNode>,
}

impl MengerNode {
    fn leaf(bounds: WorldBounds, depth: u32, is_corridor: bool) -> Self {
        Self {
            bounds,
            depth,
            is_corridor,
            children: Vec::new(),
        }
    }

    /// Depth-first visit of every node in the partition.
    pub fn visit(&self, f: &mut dyn FnMut(&MengerNode)) {
        f(self);
        for child in &self.children {
            child.visit(f);
        }
    }
}

/// Fraction of ring cells that subdivide instead of staying one room.
const SUBDIVIDE_CHANCE: f32 = 0.65;

/// Splits `bounds` into the 3x3 child lattice, row-major in (x, z).
fn subdivide_thirds(bounds: WorldBounds) -> [WorldBounds; 9] {
    let dx = (bounds.max_x - bounds.min_x) / 3.0;
    let dz = (bounds.max_z - bounds.min_z) / 3.0;
    std::array::from_fn(|i| {
        let (ix, iz) = ((i % 3) as f32, (i / 3) as f32);
        WorldBounds::new(
            bounds.min_x + ix * dx,
            bounds.min_z + iz * dz,
            bounds.min_x + (ix + 1.0) * dx,
            bounds.min_z + (iz + 1.0) * dz,
        )
    })
}

/// A cell may only subdivide while each of its nine children can still hold
/// a walkable corridor.
fn subdividable(bounds: WorldBounds) -> bool {
    let child_x = (bounds.max_x - bounds.min_x) / 3.0;
    let child_z = (bounds.max_z - bounds.min_z) / 3.0;
    child_x.min(child_z) >= CAD_CORRIDOR_WIDTH
}

/// Generates the partition for one floor plate. `max_depth` bounds the
/// recursion; span limits stop it earlier wherever cells shrink to human
/// scale first.
pub fn generate_menger_bsp(bounds: WorldBounds, max_depth: u32, seed: u32) -> MengerNode {
    generate_recursive(bounds, 0, max_depth, seed)
}

fn generate_recursive(bounds: WorldBounds, depth: u32, max_depth: u32, seed: u32) -> MengerNode {
    if depth >= max_depth || !subdividable(bounds) {
        return MengerNode::leaf(bounds, depth, false);
    }

    let children = subdivide_thirds(bounds)
        .into_iter()
        .enumerate()
        .map(|(i, child_bounds)| {
            if i == 4 {
                // The Menger cutout: center cell becomes circulation.
                return MengerNode::leaf(child_bounds, depth + 1, true);
            }
            // Ring cells decide from their own world position, never their
            // parent's identity, so overlapping queries agree.
            let roll = hash(
                seed,
                0x3E9C_E11A_0000_0000 ^ u64::from(depth),
                child_bounds.min_x.floor() as i64,
                child_bounds.min_z.floor() as i64,
            );
            if unit(roll) < SUBDIVIDE_CHANCE {
                generate_recursive(child_bounds, depth + 1, max_depth, seed)
            } else {
                MengerNode::leaf(child_bounds, depth + 1, false)
            }
        })
        .collect();

    MengerNode {
        bounds,
        depth,
        is_corridor: false,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plate() -> WorldBounds {
        WorldBounds::new(0.0, 0.0, 81.0, 81.0)
    }

    fn collect<'a>(root: &'a MengerNode) -> Vec<&'a MengerNode> {
        let mut nodes = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            nodes.push(node);
            stack.extend(node.children.iter());
        }
        nodes
    }

    #[test]
    fn children_tile_their_parent_exactly() {
        let root = generate_menger_bsp(plate(), 3, 42);
        for node in collect(&root) {
            if node.children.is_empty() {
                continue;
            }
            assert_eq!(node.children.len(), 9);
            let parent_area =
                (node.bounds.max_x - node.bounds.min_x) * (node.bounds.max_z - node.bounds.min_z);
            let child_area: f32 = node
                .children
                .iter()
                .map(|c| (c.bounds.max_x - c.bounds.min_x) * (c.bounds.max_z - c.bounds.min_z))
                .sum();
            assert!(
                (parent_area - child_area).abs() < parent_area * 1e-4,
                "children must cover the parent: {parent_area} vs {child_area}"
            );
        }
    }

    #[test]
    fn every_subdivision_carves_a_central_corridor() {
        let root = generate_menger_bsp(plate(), 3, 42);
        for node in collect(&root) {
            if !node.children.is_empty() {
                assert!(node.children[4].is_corridor, "center cell must circulate");
            }
        }
    }

    #[test]
    fn partition_is_deterministic_and_seed_sensitive() {
        let shape = |seed: u32| {
            let mut signature = Vec::new();
            generate_menger_bsp(plate(), 3, seed).visit(&mut |node| {
                signature.push((node.depth, node.is_corridor, node.children.len()));
            });
            signature
        };
        assert_eq!(shape(42), shape(42));
        assert_ne!(shape(42), shape(43), "seed must matter");
    }

    #[test]
    fn recursion_never_produces_an_unwalkable_corridor() {
        let root = generate_menger_bsp(plate(), 10, 7);
        for node in collect(&root) {
            if node.is_corridor {
                let width = (node.bounds.max_x - node.bounds.min_x)
                    .min(node.bounds.max_z - node.bounds.min_z);
                assert!(
                    width >= CAD_CORRIDOR_WIDTH - 1e-4,
                    "corridor narrower than a human passage: {width}"
                );
            }
        }
    }
}
