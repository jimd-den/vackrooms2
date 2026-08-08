//! Region-scale architectural planning for Backrooms Level 0.
//!
//! The world is tiled by fixed 80 u square regions on a world-space lattice.
//! [`generate_region_plan`] is a *pure* function of `(seed, region)`: it
//! derives the region's designers ([`ArchitectGenome`]), routes a dominant
//! circulation spine first (between edge portals shared with the neighboring
//! regions), attaches incomplete program masses ([`AssemblyInstance`]) beside
//! that circulation,
//! gives them structure / ceilings / fixtures, and finally applies a
//! Backrooms corruption pass (duplicated suites, abandoned expansions,
//! renovation overlays).
//!
//! Cross-region continuity: a corridor leaves a region only at a *portal* on
//! a shared edge, and the portal position is derived from the *edge's*
//! lattice coordinate — both neighbors compute the identical portal, so
//! corridors chain across regions forever (a main corridor that never ends
//! is the first and cheapest corruption).
//!
//! Dimensions and split planes use a 0.4 u lattice. Hosted wall centerlines
//! may use its 0.2 u half-step, which aligns them with coarse voxel centers
//! without changing the physical plan at different LODs.

use crate::domain::entities::architecture::*;
use crate::domain::entities::position::Position;
use crate::use_cases::anomaly_plan::plan_anomalies_for_region;
use crate::use_cases::generate_chunk::GeneratorConfig;
use crate::use_cases::ports::NoiseProvider;

/// Side of a region, world units. A multiple of both chunk sizes (10/20).
pub const REGION_SIZE: f32 = 80.0;
/// Plan wall thickness: an institutional double-wythe partition (two CAD
/// interior walls back to back). The value must also stay >= one coarse
/// voxel (0.4) or walls vanish at the LOD-1 proxy resolution.
pub const PLAN_WALL_T: f32 = 2.0 * crate::domain::entities::cad::CAD_WALL_THICKNESS;
/// Doorways are cased rough openings around the referencable CAD single
/// door: finished width plus 0.15 u of jamb clearance per side, so the
/// coarsest supported voxel scale (0.4 u) still quantizes the opening
/// wider than the player capsule.
pub const DOOR_JAMB_CLEARANCE: f32 = 0.15;
pub const DOOR_WIDTH: f32 =
    crate::domain::entities::cad::CAD_DOOR_WIDTH + 2.0 * DOOR_JAMB_CLEARANCE;
/// Lintels sit at the standard CAD door-frame height.
pub const DOOR_HEIGHT: f32 = crate::domain::entities::cad::CAD_DOOR_HEIGHT;
/// Snap lattice for all plan geometry (= coarsest voxel size).
const SNAP: f32 = 0.4;
/// Keep-out margin from region edges for assembly footprints.
const EDGE_MARGIN: f32 = 3.2;

/// Region lattice index of a world coordinate.
pub fn region_index(w: f32) -> i64 {
    (w / REGION_SIZE).floor() as i64
}

fn snap(v: f32) -> f32 {
    (v / SNAP).round() * SNAP
}

/// Corridor widths snap to 0.8 so *half*-widths stay on the 0.4 lattice and
/// suite front walls land flush against corridor edges.
fn snap_width(v: f32) -> f32 {
    ((v / (2.0 * SNAP)).round() * 2.0 * SNAP).max(2.0 * SNAP)
}

// ---------------------------------------------------------------------------
// Deterministic hashing and edge portals.
//
// All "randomness" flows through `world_topology::hash01`, and the portal a
// corridor uses to cross a region border is the macro graph's portal — one
// derivation shared by both neighbors of the edge and by the topology layer.
// ---------------------------------------------------------------------------

use crate::use_cases::world_topology::{hash01, primary_portal_z as v_edge_portal_z};

fn pick_index(h: f32, len: usize) -> usize {
    ((h * len as f32) as usize).min(len - 1)
}

/// World spawn: a point *on the main corridor centerline* of region (0, 0),
/// a few units east of its west portal. The first thing the player sees is
/// therefore the region's dominant circulation route — walls running away in
/// both directions — rather than unplanned fabric. Both the generator core
/// and the browser composition root derive spawn from this single function,
/// so the player and the voxelizer can never disagree about where "here" is.
pub fn spawn_point(seed: u32) -> Position {
    // The main spine's west leg always runs from (0, west_z) to at least
    // x = 0.35 * REGION_SIZE, so a few units in we are guaranteed to stand
    // on a straight, readable stretch of corridor.
    Position::new(6.0, v_edge_portal_z(seed, 0, 0))
}

// ---------------------------------------------------------------------------
// The planning stages, one intent per module.
// ---------------------------------------------------------------------------

mod circulation;
mod corruption;
mod debug;
mod genome;
mod suites;
// Phase 2 steps 2-3: free-space decomposition and scored greedy placement.
// These drive `generate_region_plan` -- see the placement pass below.
mod layout;
mod territories;
#[cfg(test)]
mod tests;

pub use debug::debug_region_ascii;
pub use genome::derive_genome;

use circulation::build_corridors;
use corruption::corrupt;

/// Pure, deterministic plan for the region whose lower-left corner is
/// `region_origin` (must lie on the region lattice).
pub fn generate_region_plan(
    seed: u32,
    region_origin: Position,
    region_size: f32,
    config: &GeneratorConfig,
    noise: &dyn NoiseProvider,
) -> RegionPlan {
    let rx = region_index(region_origin.x + 0.1);
    let rz = region_index(region_origin.z + 0.1);
    let h = |k: i64| hash01(seed, &[0xA0 + k, rx, rz]);

    let dominant = derive_genome(seed, rx, rz, 0, noise);
    let renovator = derive_genome(seed, rx, rz, 1, noise);
    let mut architects = vec![dominant.clone(), renovator];
    // A third designer intrudes where the style-blend field runs high: the
    // macro fields decide *where* cultures leak into each other; the hash
    // only decides whether this particular region exposes the seam.
    let fields = crate::use_cases::world_topology::sample_fields(
        noise,
        seed,
        (rx as f32 + 0.5) * REGION_SIZE,
        (rz as f32 + 0.5) * REGION_SIZE,
    );
    if h(0) < 0.10 + 0.55 * fields.style_blend {
        architects.push(derive_genome(seed, rx, rz, 2, noise));
    }

    let corridors = build_corridors(seed, rx, rz, &dominant, region_origin, region_size);

    // --- incomplete masses beside circulation -------------------------------
    // Scored greedy placement over the region's free space: decompose what
    // circulation leaves, then let each program in the budget simulate every
    // seat on every leg and commit the best. Every spine offers legs, not
    // only the dominant one -- a secondary hall with nothing on it is a
    // corridor to nowhere, which reads as liminal only when authored.
    let mut assemblies: Vec<AssemblyInstance> = Vec::new();
    let mut taken: Vec<(f32, f32, f32, f32)> = Vec::new();
    let mut id = 0u32;
    let placement_legs = layout::legs_of(&corridors, 14.0);
    let territories = territories::free_territories(
        region_origin.x,
        region_origin.z,
        region_size,
        &corridors,
        &taken,
        EDGE_MARGIN,
        PLAN_WALL_T,
    );
    assemblies.extend(layout::lay_out_suites(
        &|k| hash01(seed, &[k, rx, rz]),
        &dominant,
        &placement_legs,
        &territories,
        region_origin,
        region_size,
        PLAN_WALL_T,
        &corridors,
        &mut taken,
        &mut id,
    ));

    // The stair placer wants raw leg spans, not placement candidates.
    let legs: Vec<(f32, f32, f32, f32)> = placement_legs
        .iter()
        .map(|l| (l.x0, l.x1, l.z, l.half_width * 2.0))
        .collect();

    // --- corruption pass ----------------------------------------------------
    corrupt(
        seed,
        rx,
        rz,
        &dominant,
        config,
        noise,
        &mut assemblies,
        &mut taken,
        &corridors,
    );

    // --- vertical circulation ------------------------------------------------
    // The macro graph may have reserved a stairwell here. Upward links are
    // realized as a Stair assembly beside the main spine; downward links
    // stay graph reservations until the engine streams below elevation 0.
    // Placed after corruption so no pass can duplicate, abandon, or redden
    // a stair core.
    if let Some(link) =
        crate::use_cases::world_topology::vertical_link_for_region(seed, noise, rx, rz)
        && crate::use_cases::vertical_circulation::link_wants_geometry(&link)
        && let Some(stair) = crate::use_cases::vertical_circulation::place_stairwell(
            id + 2000,
            &link,
            &legs,
            region_origin,
            region_size,
            EDGE_MARGIN,
            PLAN_WALL_T,
            &taken,
            |cx, cz, clearance| {
                corridors
                    .iter()
                    .all(|s| s.distance(cx, cz) >= s.width * 0.5 + clearance)
            },
        )
    {
        taken.push(stair.footprint.bounds());
        assemblies.push(stair);
    }

    // Macro anomalies are derived after ordinary architecture so compact red
    // rooms can promote a real assembly, while region-spanning families keep
    // stable world-lattice identities independent of this region query.
    let anomalies = plan_anomalies_for_region(
        seed,
        region_origin,
        region_size,
        &assemblies,
        spawn_point(seed),
        config,
        noise,
    );

    RegionPlan {
        origin_world: region_origin,
        size_world: region_size,
        architects,
        assemblies,
        corridors,
        anomalies,
    }
}
