//! Human-proportioned architectural constants: a referencable ergonomic
//! vocabulary for CAD-grade construction (CSG cuts, WFC module authoring,
//! macro partitioning) and for architects using the engine as a sandbox.
//!
//! Values are in world units (1 unit = 1 meter) and follow common US
//! building-code dimensions (IBC egress, ADA clearances) rounded to
//! centimeters. They are *authoring* constants for new construction paths:
//! the live Level 0 generator keeps its own historical dimensions (doorways
//! 2.2 u, fabric ceilings 2.6-5.4 u), and rewiring it onto these values
//! would change every generated world byte — a deliberate, separately
//! tested change, not a refactor.

// -- doors -------------------------------------------------------------------

/// Standard interior door frame height (6'10" nominal).
pub const CAD_DOOR_HEIGHT: f32 = 2.1;
/// Standard single-leaf interior door width.
pub const CAD_DOOR_WIDTH: f32 = 0.9;
/// ADA minimum clear opening width (32 in).
pub const CAD_DOOR_WIDTH_ADA_MIN: f32 = 0.815;
/// Paired egress doors.
pub const CAD_DOOR_WIDTH_DOUBLE: f32 = 1.8;

// -- circulation -------------------------------------------------------------

/// Comfortable two-person corridor; also the sandbox default.
pub const CAD_CORRIDOR_WIDTH: f32 = 1.8;
/// IBC minimum egress corridor width (44 in).
pub const CAD_CORRIDOR_WIDTH_EGRESS_MIN: f32 = 1.12;
/// Residential hallway minimum (36 in).
pub const CAD_CORRIDOR_WIDTH_RESIDENTIAL_MIN: f32 = 0.9;

// -- vertical section --------------------------------------------------------

/// Institutional drop-ceiling height; the liminal-office default.
pub const CAD_STANDARD_CEILING: f32 = 3.0;
/// Habitable-room minimum ceiling (IBC 7'6").
pub const CAD_CEILING_MIN_HABITABLE: f32 = 2.29;
/// Interior partition wall thickness (2x4 stud + gypsum both sides).
pub const CAD_WALL_THICKNESS: f32 = 0.2;

// -- stairs ------------------------------------------------------------------

/// IBC maximum stair riser height (7 in).
pub const CAD_STAIR_RISER_MAX: f32 = 0.178;
/// IBC minimum stair tread depth (11 in).
pub const CAD_STAIR_TREAD_MIN: f32 = 0.279;
/// Handrail height above nosing (34-38 in band midpoint).
pub const CAD_HANDRAIL_HEIGHT: f32 = 0.91;

// -- predicates --------------------------------------------------------------

/// Can a human-sized agent pass through an opening of this size?
pub const fn fits_human_passage(width: f32, height: f32) -> bool {
    width >= CAD_DOOR_WIDTH_ADA_MIN && height >= CAD_DOOR_HEIGHT
}

/// Does a stair step meet the egress riser/tread envelope?
pub const fn stair_step_is_code(riser: f32, tread: f32) -> bool {
    riser > 0.0 && riser <= CAD_STAIR_RISER_MAX && tread >= CAD_STAIR_TREAD_MIN
}

/// Smallest room span that still holds a standard door plus a jamb wall on
/// both sides — the stop condition for recursive space partitioning.
pub const fn min_partitionable_span() -> f32 {
    CAD_DOOR_WIDTH + 2.0 * CAD_WALL_THICKNESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passage_check_accepts_doors_and_rejects_crawlspaces() {
        assert!(fits_human_passage(CAD_DOOR_WIDTH, CAD_DOOR_HEIGHT));
        assert!(fits_human_passage(CAD_DOOR_WIDTH_ADA_MIN, CAD_DOOR_HEIGHT));
        assert!(fits_human_passage(CAD_CORRIDOR_WIDTH, CAD_STANDARD_CEILING));
        assert!(!fits_human_passage(0.5, CAD_DOOR_HEIGHT));
        assert!(!fits_human_passage(CAD_DOOR_WIDTH, 1.2));
    }

    #[test]
    fn code_stairs_pass_and_ladders_fail() {
        assert!(stair_step_is_code(0.17, 0.28));
        assert!(!stair_step_is_code(0.25, 0.28), "riser too tall");
        assert!(!stair_step_is_code(0.17, 0.2), "tread too shallow");
    }

    #[test]
    fn dimension_orderings_stay_sane() {
        assert!(CAD_DOOR_WIDTH_ADA_MIN < CAD_DOOR_WIDTH);
        assert!(CAD_CORRIDOR_WIDTH_RESIDENTIAL_MIN < CAD_CORRIDOR_WIDTH_EGRESS_MIN);
        assert!(CAD_CEILING_MIN_HABITABLE < CAD_STANDARD_CEILING);
        assert!(min_partitionable_span() > CAD_DOOR_WIDTH);
    }
}
