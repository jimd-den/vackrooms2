//! Constructive Solid Geometry kernel: SketchUp-style authoring verbs
//! (extrude, cut, combine) over BSP-tree booleans, dependency-free and
//! platform-free.
//!
//! The pipeline an architect's action takes:
//!
//! ```text
//! sketch profile --extrude--> Solid --subtract/union--> Solid
//!                                                        |
//!                                     contains_point / bsp()  --> voxelizer
//! ```
//!
//! The voxel bridge (`use_cases::csg_voxelizer`) samples a finished solid
//! straight into the SVO builder, so authored structures enter the world
//! through the same octree path as generated architecture.

pub mod bsp;
pub mod plane;
pub mod polygon;
pub mod solid;
pub mod vec3;

pub use bsp::BspNode;
pub use plane::Plane;
pub use polygon::Polygon;
pub use solid::Solid;
pub use vec3::{Vec3, v3};
