/// Represents a dense 3D grid of voxels.
/// This is the foundation for our "smaller voxels" requirement.
pub struct VoxelGrid {
    width: usize,
    height: usize,
    depth: usize,
    data: Vec<u8>,
    /// Scalar light level 0-15 (the max of the RGB channels), kept for
    /// presenters and bakes that only need brightness.
    light_data: Vec<u8>,
    /// Colored flood-fill light, 0-15 per channel.
    light_rgb: Vec<[u8; 3]>,
    /// Six neighbor-occupancy bits prepared by the geometry source. A set bit
    /// means that face touches solid matter; clearing it means the face is
    /// exposed to air. Streaming generation computes this from its halo, so
    /// chunk-boundary answers do not depend on which neighbor is resident.
    face_occlusion: Vec<u8>,
}

/// Neighbor-occupancy encoding shared by meshing, SVO serialization, and CPU
/// surface reconstruction. The order intentionally matches the historical
/// `FaceDirection::occlusion_bit` contract.
pub const FACE_OCCLUDED_POSITIVE_X: u8 = 1;
pub const FACE_OCCLUDED_NEGATIVE_X: u8 = 2;
pub const FACE_OCCLUDED_POSITIVE_Y: u8 = 4;
pub const FACE_OCCLUDED_NEGATIVE_Y: u8 = 8;
pub const FACE_OCCLUDED_POSITIVE_Z: u8 = 16;
pub const FACE_OCCLUDED_NEGATIVE_Z: u8 = 32;
pub const FACE_OCCLUSION_MASK: u8 = 0x3f;

pub const VOXEL_AIR: u8 = 0;
pub const VOXEL_WALL: u8 = 1;
pub const VOXEL_FLOOR: u8 = 2;
pub const VOXEL_CEILING: u8 = 3;
pub const VOXEL_LIGHT: u8 = 4;
pub const VOXEL_RED_WALL: u8 = 5;
/// Walkable ground cover (grassland level). Visual-only, like FLOOR.
pub const VOXEL_GRASS: u8 = 6;
/// Walkable shallow water (grassland lakes). Visual-only, like FLOOR.
pub const VOXEL_WATER: u8 = 7;
/// Tree trunk (grassland). Solid: blocks the player like WALL.
pub const VOXEL_TREE: u8 = 8;
pub const VOXEL_RED_LIGHT: u8 = 9;
/// Pale bone-tinted arch-room wall. Solid like WALL.
pub const VOXEL_PALE_WALL: u8 = 10;
/// Rough, damaged blackout-expanse wall. Solid like WALL.
pub const VOXEL_DAMAGED_WALL: u8 = 11;
/// Shallow, dry, desaturated pillar-expanse carpet. Walkable, like FLOOR.
pub const VOXEL_DRY_CARPET: u8 = 12;
/// Deep, wet arch-room carpet. Walkable, like FLOOR.
pub const VOXEL_DEEP_CARPET: u8 = 13;
/// Thick, sticky, discolored red-room carpet. Walkable, like FLOOR.
pub const VOXEL_STICKY_CARPET: u8 = 14;
/// Recessed basin holding dark ankle-deep fluid. Walkable, like FLOOR.
pub const VOXEL_FLUID: u8 = 15;
/// Tiny cool emergency glimmer fixture (blackout recovery skeleton).
/// Emissive like LIGHT, far dimmer and colder.
pub const VOXEL_GLIMMER: u8 = 16;
/// Bare structural concrete (Level 1 walls and pillars). Solid like WALL.
pub const VOXEL_CONCRETE_WALL: u8 = 17;
/// Institutional tile floor (Level 1). Walkable, like FLOOR.
pub const VOXEL_TILE_FLOOR: u8 = 18;
/// Poured concrete slab floor (Level 1 parking sectors). Walkable.
pub const VOXEL_CONCRETE_FLOOR: u8 = 19;
/// Wooden supply crate (Level 1 Gild sector). Solid: blocks like WALL.
pub const VOXEL_CRATE: u8 = 20;
/// Exposed ceiling pipe / rebar (Level 1). Solid, but authored overhead
/// or as thin obstructions the player walks around.
pub const VOXEL_PIPE: u8 = 21;
/// Metal fire-exit door panel. Non-solid: crossing it is a level transit,
/// so the panel must admit the player who pushes into it.
pub const VOXEL_METAL_DOOR: u8 = 22;
/// Almond water bottle marker. Non-solid, faintly emissive so bottles
/// glint in dim fabric; the pickup itself is the exported `SupplyItem`.
pub const VOXEL_ALMOND_WATER: u8 = 23;
/// Duller, browner ordinary Level 0 wallpaper: `institution_age` past
/// threshold. Solid like WALL, same material class, just older.
pub const VOXEL_AGED_WALLPAPER: u8 = 24;
/// Duller, blotchier ordinary Level 0 carpet, paired with
/// `VOXEL_AGED_WALLPAPER`. Walkable, like FLOOR.
pub const VOXEL_STAINED_CARPET: u8 = 25;

// -- fit-out assembly layers -------------------------------------------------
// A wall in a real building is not one material floor-to-ceiling, and a
// dropped ceiling is not one plane. These are the layers a construction
// section would draw: the base at the bottom of a partition, the suspended
// ceiling's grid and tile, and what the grid hides above it.

/// Vinyl/rubber base at the foot of a partition, ~0.1 u tall. The single
/// most legible "this is a fitted-out building, not a maze" detail: every
/// finished wall in an office has one, and its absence is why bare voxel
/// walls read as terrain.
pub const VOXEL_BASEBOARD: u8 = 26;
/// Exposed T-bar of a suspended ceiling grid — the aluminium runners that
/// carry the tiles. Reads as the ceiling module line in a reflected ceiling
/// plan.
pub const VOXEL_CEILING_GRID: u8 = 27;
/// Galvanized supply duct in the plenum, seen through a missing tile.
/// Distinct from `VOXEL_PIPE`: duct is broad and rectangular, pipe is a run.
pub const VOXEL_DUCT: u8 = 28;
/// The structural slab over the plenum — the underside of the floor above.
/// This is the surface Level 1 exposes directly; here it is what a missing
/// ceiling tile reveals.
pub const VOXEL_SLAB: u8 = 29;

// -- fit-out on the floor ----------------------------------------------------

/// Work surfaces and casework: desks, tables, cabinets. Office laminate.
pub const VOXEL_FURNITURE: u8 = 30;
/// Seating. Separated from casework only because a room of one flat colour
/// reads as extruded blocks; the chair beside a desk is what makes both
/// legible as objects.
pub const VOXEL_SEAT: u8 = 31;
/// Shelving and racking — storage and server rooms.
pub const VOXEL_SHELVING: u8 = 32;

/// Number of voxel material ids (the palette table length).
pub const VOXEL_MATERIAL_COUNT: usize = 33;

/// Materials that block the player and produce collision boxes. Everything
/// else is walkable or decorative.
pub const SOLID_MATERIALS: [u8; 13] = [
    VOXEL_WALL,
    VOXEL_TREE,
    VOXEL_RED_WALL,
    VOXEL_PALE_WALL,
    VOXEL_DAMAGED_WALL,
    VOXEL_CONCRETE_WALL,
    VOXEL_CRATE,
    VOXEL_PIPE,
    VOXEL_AGED_WALLPAPER,
    // The base is the bottom course of a partition: it must block exactly
    // like the wall it belongs to, or the player walks through the foot of
    // every finished wall in the level.
    VOXEL_BASEBOARD,
    // Furniture is furniture: you walk around a desk, not through it.
    VOXEL_FURNITURE,
    VOXEL_SEAT,
    VOXEL_SHELVING,
];

/// Materials that make up a vertical wall surface, whatever their finish.
///
/// A wall is an *assembly*, not a material: the same partition is aged
/// wallpaper where the institution has decayed, crimson inside a red room,
/// and base at its bottom course. Code asking "is there a wall here" —
/// LOD correspondence, corridor probes, blueprint tracing — means the
/// assembly, not one finish, and every time that was written as an equality
/// against `VOXEL_WALL` it broke the next time a finish was added. Ask
/// [`is_wall_surface`] instead.
pub const WALL_SURFACE_MATERIALS: [u8; 7] = [
    VOXEL_WALL,
    VOXEL_AGED_WALLPAPER,
    VOXEL_PALE_WALL,
    VOXEL_DAMAGED_WALL,
    VOXEL_RED_WALL,
    VOXEL_CONCRETE_WALL,
    VOXEL_BASEBOARD,
];

/// Is this material part of a wall assembly, in any finish?
pub const fn is_wall_surface(material: u8) -> bool {
    let mut i = 0;
    while i < WALL_SURFACE_MATERIALS.len() {
        if WALL_SURFACE_MATERIALS[i] == material {
            return true;
        }
        i += 1;
    }
    false
}

/// Materials that emit light in the baked flood fill and render emissive.
pub const EMISSIVE_MATERIALS: [u8; 4] = [
    VOXEL_LIGHT,
    VOXEL_RED_LIGHT,
    VOXEL_GLIMMER,
    VOXEL_ALMOND_WATER,
];

/// Authored emitted-radiance scale shared by fixture extraction and every
/// renderer's visible-emission path.
pub const fn material_emission_strength(material: u8) -> Option<f32> {
    match material {
        VOXEL_LIGHT => Some(10.0),
        VOXEL_RED_LIGHT => Some(8.0),
        VOXEL_GLIMMER => Some(0.9),
        // A bottle is a glint, not a lamp: just enough to catch the eye.
        VOXEL_ALMOND_WATER => Some(0.35),
        _ => None,
    }
}

impl VoxelGrid {
    pub fn new(width: usize, height: usize, depth: usize) -> Self {
        let size = width * height * depth;
        Self {
            width,
            height,
            depth,
            data: vec![VOXEL_AIR; size],
            light_data: vec![0; size],
            light_rgb: vec![[0; 3]; size],
            face_occlusion: vec![0; size],
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn depth(&self) -> usize {
        self.depth
    }

    fn index(&self, x: usize, y: usize, z: usize) -> Option<usize> {
        if x < self.width && y < self.height && z < self.depth {
            Some(y * (self.width * self.depth) + z * self.width + x)
        } else {
            None
        }
    }

    pub fn set(&mut self, x: usize, y: usize, z: usize, voxel_type: u8) {
        if let Some(idx) = self.index(x, y, z) {
            self.data[idx] = voxel_type;
        }
    }

    pub fn get(&self, x: usize, y: usize, z: usize) -> u8 {
        if let Some(idx) = self.index(x, y, z) {
            self.data[idx]
        } else {
            VOXEL_AIR
        }
    }

    /// Sets a neutral (white) light level: all three channels get `level`.
    pub fn set_light(&mut self, x: usize, y: usize, z: usize, level: u8) {
        self.set_light_rgb(x, y, z, [level; 3]);
    }

    pub fn get_light(&self, x: usize, y: usize, z: usize) -> u8 {
        if let Some(idx) = self.index(x, y, z) {
            self.light_data[idx]
        } else {
            0
        }
    }

    /// Sets colored light (0-15 per channel); the scalar level becomes the
    /// max of the channels.
    pub fn set_light_rgb(&mut self, x: usize, y: usize, z: usize, rgb: [u8; 3]) {
        if let Some(idx) = self.index(x, y, z) {
            self.light_rgb[idx] = rgb;
            self.light_data[idx] = rgb[0].max(rgb[1]).max(rgb[2]);
        }
    }

    pub fn get_light_rgb(&self, x: usize, y: usize, z: usize) -> [u8; 3] {
        if let Some(idx) = self.index(x, y, z) {
            self.light_rgb[idx]
        } else {
            [0; 3]
        }
    }

    /// Stores the six-bit neighbor-occupancy mask for one voxel.
    pub fn set_face_occlusion(&mut self, x: usize, y: usize, z: usize, mask: u8) {
        if let Some(idx) = self.index(x, y, z) {
            self.face_occlusion[idx] = mask & FACE_OCCLUSION_MASK;
        }
    }

    pub fn get_face_occlusion(&self, x: usize, y: usize, z: usize) -> u8 {
        if let Some(idx) = self.index(x, y, z) {
            self.face_occlusion[idx]
        } else {
            0
        }
    }
}
